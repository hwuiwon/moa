//! Bounded persisted completion scanning and exact verifier materialization.

mod coverage;

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::{CompletionCheckKind, ExecutionFailureClass, RetryPolicy};
use moa_config::ExecutionConfig;
use serde::{Deserialize, Serialize};

use self::coverage::{
    CoverageTaskEvidence, PersistedCoverageEvaluation, accumulate_task_coverage_evidence,
    coverage_by_node, load_persisted_coverage_evaluations, prepare_coverage_evidence,
    resolve_coverage_expectation,
};
use super::*;
use super::{
    materialize::prepare_task_materialization_batch,
    rows::*,
    run::enqueue_run_activation_in_conn,
    sql::*,
    terminal::{ReplanStopReceipt, RunFinalizationRequest},
};
use crate::{
    completion::{
        CompletionCheckResult, CompletionEvaluation, CompletionStatus, execution_terminal_reason,
        terminal_evidence_from_evaluation,
    },
    interpreter::verifier_turn_reservation,
    replan::{replan_stop_gaps, replan_stop_status},
    repository::replan_stop::ExecutionReplanStopIntentRecord,
    schema::validate_instance,
    state::{ExecutionLimitStop, ExecutionTaskFailure},
};

const MAX_COMPLETION_PAGE_SIZE: u32 = 1_000;
const MAX_EVIDENCE_SAMPLES_PER_CHECK: usize = 20;

/// One generation- and wake-fenced bounded completion advance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionAdvanceRequest {
    /// Run whose ordinary terminal projection is being evaluated.
    pub run_uid: Uuid,
    /// Exact run-controller generation owning the scan.
    pub controller_generation: u64,
    /// Exact claimed wake that may produce terminal intent.
    pub wake_epoch: u64,
    /// Maximum forward task rows inspected by this call.
    pub page_size: u32,
    /// Deterministic evaluation time supplied by the controller.
    pub now: DateTime<Utc>,
}

/// Result of one bounded persisted completion advance.
#[derive(Clone, Debug, PartialEq)]
pub enum CompletionAdvanceOutcome {
    /// More bounded evidence or verifier settlement is required.
    Continue {
        /// Forward task rows durably scanned by this call.
        scanned_tasks: u32,
        /// Plan-node aggregate rows durably scanned by this call.
        scanned_nodes: u32,
    },
    /// A ReplanStop page committed together with the only valid next controller wake.
    ReplanStopContinue {
        /// Forward task rows durably scanned by this call.
        scanned_tasks: u32,
        /// Plan-node aggregate rows durably scanned by this call.
        scanned_nodes: u32,
        /// Exact new run-activation dispatch bound to the persisted intent.
        continuation: Box<ExecutionDispatchRecord>,
    },
    /// Exact verifier tasks were inserted into the ordinary ready queue.
    VerifiersMaterialized {
        /// Persisted verifier tasks in declared check order.
        tasks: Vec<ExecutionTaskRecord>,
    },
    /// Verifier tasks exist but have not settled; their task wake will reactivate the run.
    WaitingForVerifiers,
    /// Every ordinary completion gate passed and can be finalized atomically.
    FinalizationReady(Box<RunFinalizationRequest>),
    /// The same scan already observed a non-success terminal intent.
    NonSuccessTerminal {
        /// Fully prepared deterministic terminal intent for the compensation boundary.
        pending_terminal: PendingExecutionTerminal,
    },
    /// Bounded ReplanStop evaluation is ready for its exact terminal fence and receipt.
    ReplanStopReady {
        /// Fully prepared deterministic terminal intent.
        pending_terminal: PendingExecutionTerminal,
        /// Exact task/revision/amendment receipt owned by the persisted intent.
        receipt: ReplanStopReceipt,
    },
    /// Run, generation, wake, or ordinary terminal boundary is no longer current.
    NotReady,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct CompletionTaskEvidence {
    authorization_denied: bool,
    unsupported_by_requirement: BTreeMap<String, UnsupportedRequirementEvidence>,
    citation_failures: BTreeMap<String, CitationFailureEvidence>,
    coverage: BTreeMap<String, CoverageTaskEvidence>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UnsupportedRequirementEvidence {
    task_count: u64,
    unsupported_task_count: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CitationFailureEvidence {
    failure_count: u64,
    samples: Vec<CitationFailureSample>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CitationFailureSample {
    node_id: String,
    item_key: String,
    count: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct CompletionNodeEvidence {
    terminal_output: Option<Value>,
    requirements: BTreeMap<String, RequirementNodeEvidence>,
    required_checks: BTreeMap<String, RequiredNodeCheckEvidence>,
    coverage_passed: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RequirementNodeEvidence {
    eligible_node_count: u64,
    completed_node_count: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RequiredNodeCheckEvidence {
    observed_node_count: u64,
    failed_node_ids: Vec<String>,
}

impl ExecutionRepository {
    /// Advances at most one bounded page of terminal evidence and materializes verifiers exactly once.
    pub async fn advance_completion_projection(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: CompletionAdvanceRequest,
    ) -> Result<CompletionAdvanceOutcome> {
        self.advance_completion_projection_inner(scope, config, request, None)
            .await
    }

    /// Advances one bounded ReplanStop completion page from its exact persisted controller intent.
    pub async fn advance_replan_stop_completion_projection(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: CompletionAdvanceRequest,
        intent: &ExecutionReplanStopIntentRecord,
    ) -> Result<CompletionAdvanceOutcome> {
        self.advance_completion_projection_inner(scope, config, request, Some(intent))
            .await
    }

    async fn advance_completion_projection_inner(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: CompletionAdvanceRequest,
        replan_stop: Option<&ExecutionReplanStopIntentRecord>,
    ) -> Result<CompletionAdvanceOutcome> {
        let page_size = request.page_size.clamp(1, MAX_COMPLETION_PAGE_SIZE);
        let mut scanned_tasks = 0_u32;
        let mut scanned_nodes = 0_u32;
        let generation = to_i64(
            request.controller_generation,
            "completion controller generation",
        )?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(request.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::NotReady);
        };
        let run = run_from_row(&run_row)?;
        if let Some(intent) = replan_stop
            && (intent.tenant_id != run.tenant_id
                || intent.run_uid != run.run_uid
                || intent.controller_generation != request.controller_generation
                || intent.wake_epoch != request.wake_epoch
                || intent.base_plan_revision != run.plan_revision)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::NotReady);
        }
        if let Some(intent) = replan_stop {
            let persisted_intent = sqlx::query_scalar::<_, i32>(
                "SELECT 1 FROM moa.execution_replan_stop_intent \
                 WHERE tenant_id=$1 AND run_uid=$2 AND controller_generation=$3 \
                   AND wake_epoch=$4 AND origin_task_id=$5 AND task_generation=$6 \
                   AND base_plan_revision=$7 AND stop_reason=$8 AND detail=$9 \
                   AND amendment_hash=$10 FOR UPDATE",
            )
            .bind(run.tenant_id.0)
            .bind(run.run_uid)
            .bind(to_i64(
                request.controller_generation,
                "replan-stop controller generation",
            )?)
            .bind(to_i64(request.wake_epoch, "replan-stop wake epoch")?)
            .bind(intent.origin_task_id.as_uuid())
            .bind(to_i64(
                intent.task_generation,
                "replan-stop task generation",
            )?)
            .bind(to_i64(
                intent.base_plan_revision,
                "replan-stop plan revision",
            )?)
            .bind(intent.stop_reason.as_str())
            .bind(&intent.detail)
            .bind(intent.amendment_hash.to_string())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if persisted_intent.is_none() {
                conn.commit().await.map_err(storage_error)?;
                return Ok(CompletionAdvanceOutcome::NotReady);
            }
        }
        validate_completion_runtime_bounds(&run, config)?;
        if run.controller_generation != request.controller_generation
            || run.wake_epoch != request.wake_epoch
            || run.status.is_terminal()
            || run.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::NotReady);
        }
        let excluded_task_id = replan_stop.map(|intent| intent.origin_task_id.as_uuid());
        let excluded_node_id = if let Some(intent) = replan_stop {
            let row = sqlx::query(
                "SELECT node_id,generation,status FROM moa.execution_task \
                 WHERE run_uid=$1 AND task_id=$2",
            )
            .bind(run.run_uid)
            .bind(intent.origin_task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(CompletionAdvanceOutcome::NotReady);
            };
            if required_u64(&row, "generation")? != intent.task_generation
                || row.try_get::<String, _>("status").map_err(row_error)? != "waiting_replan"
            {
                conn.commit().await.map_err(storage_error)?;
                return Ok(CompletionAdvanceOutcome::NotReady);
            }
            Some(row.try_get::<String, _>("node_id").map_err(row_error)?)
        } else {
            None
        };
        let unfinished = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_node_state \
             WHERE run_uid=$1 AND node_id NOT LIKE '@check/%' \
               AND ($2::TEXT IS NULL OR node_id<>$2) \
               AND node_status NOT IN ('completed','skipped','failed','cancelled')) \
             OR EXISTS (SELECT 1 FROM moa.execution_task \
             WHERE run_uid=$1 AND node_id NOT LIKE '@check/%' \
               AND ($3::UUID IS NULL OR task_id<>$3) \
               AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome'))",
        )
        .bind(request.run_uid)
        .bind(&excluded_node_id)
        .bind(excluded_task_id)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if unfinished {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::NotReady);
        }

        let scan_kind = if replan_stop.is_some() {
            "replan_stop"
        } else {
            "ordinary"
        };
        sqlx::query(
            "DELETE FROM moa.execution_completion_scan WHERE run_uid=$1 \
               AND (scan_kind<>$2 OR excluded_task_id IS DISTINCT FROM $3)",
        )
        .bind(run.run_uid)
        .bind(scan_kind)
        .bind(excluded_task_id)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        sqlx::query(
            "INSERT INTO moa.execution_completion_scan (tenant_id, run_uid, plan_revision, \
                 controller_generation,scan_kind,excluded_task_id,source_progress_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7) \
             ON CONFLICT (tenant_id,run_uid) DO UPDATE SET plan_revision=EXCLUDED.plan_revision, \
                 controller_generation=EXCLUDED.controller_generation, task_cursor=NULL, \
                 scanned_task_count=0, task_evidence='{}'::JSONB, scan_complete=FALSE, \
                 node_cursor=NULL, completion_evidence='{}'::JSONB, \
                 node_scan_complete=FALSE, verifiers_materialized=FALSE, \
                 scan_kind=EXCLUDED.scan_kind,excluded_task_id=EXCLUDED.excluded_task_id, \
                 source_progress_at=EXCLUDED.source_progress_at,updated_at=NOW() \
             WHERE execution_completion_scan.plan_revision <> EXCLUDED.plan_revision \
                OR execution_completion_scan.controller_generation \
                   <> EXCLUDED.controller_generation \
                OR execution_completion_scan.source_progress_at \
                   <> EXCLUDED.source_progress_at",
        )
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(to_i64(run.plan_revision, "completion plan revision")?)
        .bind(generation)
        .bind(scan_kind)
        .bind(excluded_task_id)
        .bind(run.last_progress_at)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let scan = sqlx::query(
            "SELECT plan_revision, controller_generation,scan_kind,excluded_task_id, \
                    source_progress_at,task_cursor, scanned_task_count, \
                    task_evidence, scan_complete, node_cursor, completion_evidence, \
                    node_scan_complete, verifiers_materialized \
             FROM moa.execution_completion_scan WHERE run_uid = $1 FOR UPDATE",
        )
        .bind(run.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if required_u64(&scan, "plan_revision")? != run.plan_revision
            || required_u64(&scan, "controller_generation")? != request.controller_generation
            || scan.try_get::<String, _>("scan_kind").map_err(row_error)? != scan_kind
            || scan
                .try_get::<Option<Uuid>, _>("excluded_task_id")
                .map_err(row_error)?
                != excluded_task_id
            || scan
                .try_get::<DateTime<Utc>, _>("source_progress_at")
                .map_err(row_error)?
                != run.last_progress_at
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::NotReady);
        }
        let mut evidence: CompletionTaskEvidence = serde_json::from_value(
            scan.try_get::<Value, _>("task_evidence")
                .map_err(row_error)?,
        )?;
        let scan_complete: bool = scan.try_get("scan_complete").map_err(row_error)?;
        let mut node_evidence: CompletionNodeEvidence = serde_json::from_value(
            scan.try_get::<Value, _>("completion_evidence")
                .map_err(row_error)?,
        )?;
        let node_scan_complete: bool = scan.try_get("node_scan_complete").map_err(row_error)?;
        let verifiers_materialized: bool =
            scan.try_get("verifiers_materialized").map_err(row_error)?;
        if !scan_complete {
            let cursor: Option<Uuid> = scan.try_get("task_cursor").map_err(row_error)?;
            let rows = sqlx::query(
                "SELECT * FROM moa.execution_task WHERE run_uid = $1 \
                   AND node_id NOT LIKE '@check/%' AND ($2::UUID IS NULL OR task_id > $2) \
                 ORDER BY task_id LIMIT $3",
            )
            .bind(run.run_uid)
            .bind(cursor)
            .bind(i64::from(page_size) + 1)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let page_size_usize =
                usize::try_from(page_size).map_err(|_| Error::ArithmeticOverflow {
                    context: "completion page size".to_string(),
                })?;
            let has_more = rows.len() > page_size_usize;
            let page = rows.into_iter().take(page_size_usize).collect::<Vec<_>>();
            let tasks = page.iter().map(task_from_row).collect::<Result<Vec<_>>>()?;
            scanned_tasks = u32::try_from(tasks.len()).map_err(|_| Error::ArithmeticOverflow {
                context: "completion page task count".to_string(),
            })?;
            for coverage in &run.goal.coverage {
                let expectation =
                    resolve_coverage_expectation(conn.as_mut(), &run, coverage).await?;
                prepare_coverage_evidence(coverage, &expectation, &mut evidence)?;
                for task in &tasks {
                    accumulate_task_coverage_evidence(coverage, task, &expectation, &mut evidence)?;
                }
            }
            for task in &tasks {
                accumulate_task_evidence(&run, task, &mut evidence)?;
            }
            let next_cursor = tasks.last().map(|task| task.task_id.as_uuid()).or(cursor);
            let scanned_delta =
                u64::try_from(tasks.len()).map_err(|_| Error::ArithmeticOverflow {
                    context: "completion scanned task count".to_string(),
                })?;
            let scanned = required_u64(&scan, "scanned_task_count")?
                .checked_add(scanned_delta)
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: "completion scanned task count".to_string(),
                })?;
            sqlx::query(
                "UPDATE moa.execution_completion_scan SET task_cursor=$2, scanned_task_count=$3, \
                     task_evidence=$4, scan_complete=$5, updated_at=NOW() \
                 WHERE run_uid=$1 AND plan_revision=$6 AND controller_generation=$7",
            )
            .bind(run.run_uid)
            .bind(next_cursor)
            .bind(to_i64(scanned, "completion scanned task count")?)
            .bind(serde_json::to_value(&evidence)?)
            .bind(!has_more)
            .bind(to_i64(run.plan_revision, "completion plan revision")?)
            .bind(generation)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if has_more || !tasks.is_empty() {
                return commit_completion_page(
                    conn,
                    &run,
                    &request,
                    replan_stop,
                    scanned_tasks,
                    scanned_nodes,
                )
                .await;
            }
        }

        if !node_scan_complete {
            let cursor: Option<i64> = scan.try_get("node_cursor").map_err(row_error)?;
            let rows = sqlx::query(
                "SELECT node_id,node_order,node_status,total_task_count,succeeded_task_count, \
                        failed_task_count,cancelled_task_count,aggregate_output \
                 FROM moa.execution_node_state WHERE run_uid=$1 \
                   AND node_id NOT LIKE '@check/%' AND ($2::BIGINT IS NULL OR node_order > $2) \
                 ORDER BY node_order,node_state_uid LIMIT $3",
            )
            .bind(run.run_uid)
            .bind(cursor)
            .bind(i64::from(page_size) + 1)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let page_size_usize =
                usize::try_from(page_size).map_err(|_| Error::ArithmeticOverflow {
                    context: "completion node page size".to_string(),
                })?;
            let has_more = rows.len() > page_size_usize;
            let page = rows.into_iter().take(page_size_usize).collect::<Vec<_>>();
            scanned_nodes = u32::try_from(page.len()).map_err(|_| Error::ArithmeticOverflow {
                context: "completion page node count".to_string(),
            })?;
            for row in &page {
                accumulate_node_evidence(
                    &run,
                    row,
                    &mut node_evidence,
                    excluded_node_id.as_deref(),
                )?;
            }
            let next_cursor = page
                .last()
                .map(|row| row.try_get::<i64, _>("node_order").map_err(row_error))
                .transpose()?
                .or(cursor);
            sqlx::query(
                "UPDATE moa.execution_completion_scan SET node_cursor=$2, \
                     completion_evidence=$3,node_scan_complete=$4,updated_at=NOW() \
                 WHERE run_uid=$1 AND plan_revision=$5 AND controller_generation=$6",
            )
            .bind(run.run_uid)
            .bind(next_cursor)
            .bind(serde_json::to_value(&node_evidence)?)
            .bind(!has_more)
            .bind(to_i64(run.plan_revision, "completion plan revision")?)
            .bind(generation)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if has_more || !page.is_empty() {
                return commit_completion_page(
                    conn,
                    &run,
                    &request,
                    replan_stop,
                    scanned_tasks,
                    scanned_nodes,
                )
                .await;
            }
        }

        let verifier_checks = run
            .goal
            .completion_checks
            .iter()
            .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
            .count();
        if replan_stop.is_none() && verifier_checks > 0 && !verifiers_materialized {
            let (tasks, all_materialized) = materialize_verifiers_in_tx(
                conn.as_mut(),
                config,
                &run,
                &evidence,
                &node_evidence,
                page_size,
            )
            .await?;
            sqlx::query(
                "UPDATE moa.execution_completion_scan SET verifiers_materialized=$4, \
                     updated_at=NOW() WHERE run_uid=$1 AND plan_revision=$2 \
                     AND controller_generation=$3",
            )
            .bind(run.run_uid)
            .bind(to_i64(run.plan_revision, "completion plan revision")?)
            .bind(generation)
            .bind(all_materialized)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::VerifiersMaterialized { tasks });
        }

        if replan_stop.is_some() && !verifiers_materialized {
            sqlx::query(
                "UPDATE moa.execution_completion_scan SET verifiers_materialized=TRUE, \
                 updated_at=NOW() WHERE run_uid=$1 AND plan_revision=$2 \
                 AND controller_generation=$3 AND scan_kind='replan_stop'",
            )
            .bind(run.run_uid)
            .bind(to_i64(run.plan_revision, "completion plan revision")?)
            .bind(generation)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        }
        let verifier_tasks = if replan_stop.is_some() {
            load_existing_verifier_tasks(conn.as_mut(), config, &run).await?
        } else {
            load_verifier_tasks(conn.as_mut(), &run).await?
        };
        if replan_stop.is_none() && verifier_tasks.iter().any(|task| !task.status.is_terminal()) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::WaitingForVerifiers);
        }
        let coverage_evaluations =
            load_persisted_coverage_evaluations(conn.as_mut(), &run, &node_evidence, &evidence)
                .await?;
        let (mut evaluation, terminal_output) = evaluate_persisted_completion(
            &run,
            &node_evidence,
            &evidence,
            &coverage_evaluations,
            &verifier_tasks,
            request.now,
        )?;
        if let Some(intent) = replan_stop {
            evaluation.status = replan_stop_status(
                terminal_output.is_some(),
                evaluation.satisfied_requirement_ids.len(),
            );
            evaluation
                .gaps
                .extend(replan_stop_gaps(intent.stop_reason, Some(&intent.detail)));
            evaluation.gaps.sort();
            evaluation.gaps.dedup();
            let typed_failure =
                load_earliest_typed_task_failure(conn.as_mut(), run.run_uid).await?;
            let terminal_projection =
                terminal_projection_for_evaluation(&evaluation, terminal_output, typed_failure)?;
            let cause = ExecutionTerminalCause::ReplanStop {
                reason: intent.stop_reason,
            };
            let terminal_evidence = terminal_evidence_from_evaluation(cause, &evaluation)?;
            let reason = execution_terminal_reason(
                &terminal_evidence.cause,
                &terminal_projection,
                &evaluation,
            )?;
            let pending_terminal = PendingExecutionTerminal {
                status: crate::completion::run_status_from_completion(evaluation.status),
                reason,
                terminal_evidence,
                completion_check_results: evaluation
                    .checks
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                terminal_gaps: evaluation.gaps,
                output: node_evidence.terminal_output,
                cancellation_reason: None,
            };
            pending_terminal.validate()?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::ReplanStopReady {
                pending_terminal,
                receipt: intent.receipt(),
            });
        }
        let typed_failure = load_earliest_typed_task_failure(conn.as_mut(), run.run_uid).await?;
        let typed_failure_class = typed_failure.as_ref().map(|failure| failure.class.clone());
        let terminal_projection =
            terminal_projection_for_evaluation(&evaluation, terminal_output, typed_failure)?;
        if evaluation.status != CompletionStatus::Completed {
            let cause = typed_failure_class.map_or(
                ExecutionTerminalCause::Completion {
                    limit_stop: evaluation.limit_stop,
                },
                |class| ExecutionTerminalCause::TaskFailure { class },
            );
            let terminal_evidence = terminal_evidence_from_evaluation(cause, &evaluation)?;
            let reason = execution_terminal_reason(
                &terminal_evidence.cause,
                &terminal_projection,
                &evaluation,
            )?;
            let pending_terminal = PendingExecutionTerminal {
                status: crate::completion::run_status_from_completion(evaluation.status),
                reason,
                terminal_evidence,
                completion_check_results: evaluation
                    .checks
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                terminal_gaps: evaluation.gaps.clone(),
                output: node_evidence.terminal_output.clone(),
                cancellation_reason: None,
            };
            pending_terminal.validate()?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompletionAdvanceOutcome::NonSuccessTerminal { pending_terminal });
        }
        let terminal_evidence = terminal_evidence_from_evaluation(
            ExecutionTerminalCause::Completion {
                limit_stop: evaluation.limit_stop,
            },
            &evaluation,
        )?;
        let terminal_reason =
            execution_terminal_reason(&terminal_evidence.cause, &terminal_projection, &evaluation)?;
        let finalization = RunFinalizationRequest {
            run_uid: run.run_uid,
            expected_revision: run.plan_revision,
            expected_wake_epoch: request.wake_epoch,
            terminal_projection,
            completion_evaluation: evaluation,
            terminal_evidence,
            terminal_reason,
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(CompletionAdvanceOutcome::FinalizationReady(Box::new(
            finalization,
        )))
    }
}

async fn commit_completion_page(
    mut conn: ScopedConn<'_>,
    run: &ExecutionRunRecord,
    request: &CompletionAdvanceRequest,
    replan_stop: Option<&ExecutionReplanStopIntentRecord>,
    scanned_tasks: u32,
    scanned_nodes: u32,
) -> Result<CompletionAdvanceOutcome> {
    let Some(intent) = replan_stop else {
        conn.commit().await.map_err(storage_error)?;
        return Ok(CompletionAdvanceOutcome::Continue {
            scanned_tasks,
            scanned_nodes,
        });
    };
    let acknowledged = sqlx::query(
        "UPDATE moa.execution_run SET processed_wake_epoch=$3, \
             activation_state='idle',activation_failure_count=0,updated_at=NOW() \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch=$3 \
           AND processed_wake_epoch<$3 AND activation_state='advancing' \
         RETURNING tenant_id",
    )
    .bind(run.run_uid)
    .bind(to_i64(
        request.controller_generation,
        "completion controller generation",
    )?)
    .bind(to_i64(request.wake_epoch, "completion wake epoch")?)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if acknowledged.is_none() {
        conn.rollback().await.map_err(storage_error)?;
        return Ok(CompletionAdvanceOutcome::NotReady);
    }
    let continuation = enqueue_run_activation_in_conn(
        conn.as_mut(),
        run.tenant_id,
        run.run_uid,
        request.controller_generation,
        request.now,
        json!({
            "reason": "replan_stop_completion_continue",
            "origin_task_id": intent.origin_task_id,
            "base_plan_revision": intent.base_plan_revision,
        }),
    )
    .await?;
    let new_wake_epoch = continuation
        .wake_epoch
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "replan-stop continuation is missing its wake epoch".to_string(),
        })?;
    let rebound = sqlx::query(
        "UPDATE moa.execution_replan_stop_intent SET wake_epoch=$4,updated_at=NOW() \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch=$3 \
           AND origin_task_id=$5 AND task_generation=$6 AND base_plan_revision=$7 \
           AND amendment_hash=$8",
    )
    .bind(run.run_uid)
    .bind(to_i64(
        request.controller_generation,
        "replan-stop controller generation",
    )?)
    .bind(to_i64(request.wake_epoch, "replan-stop wake epoch")?)
    .bind(to_i64(
        new_wake_epoch,
        "replan-stop continuation wake epoch",
    )?)
    .bind(intent.origin_task_id.as_uuid())
    .bind(to_i64(
        intent.task_generation,
        "replan-stop task generation",
    )?)
    .bind(to_i64(
        intent.base_plan_revision,
        "replan-stop plan revision",
    )?)
    .bind(intent.amendment_hash.to_string())
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if rebound.rows_affected() != 1 {
        conn.rollback().await.map_err(storage_error)?;
        return Ok(CompletionAdvanceOutcome::NotReady);
    }
    conn.commit().await.map_err(storage_error)?;
    Ok(CompletionAdvanceOutcome::ReplanStopContinue {
        scanned_tasks,
        scanned_nodes,
        continuation: Box::new(continuation),
    })
}

fn accumulate_task_evidence(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    evidence: &mut CompletionTaskEvidence,
) -> Result<()> {
    let mut unsupported = false;
    if let Some(outcome) = &task.current_outcome
        && let ExecutionTaskResult::Failed { class, .. } = &outcome.result
    {
        if *class == ExecutionFailureClass::AuthorizationDenied {
            evidence.authorization_denied = true;
        }
        if *class == ExecutionFailureClass::Unsupported {
            unsupported = true;
        }
    }
    for requirement_id in &task.requirement_ids {
        let requirement = evidence
            .unsupported_by_requirement
            .entry(requirement_id.clone())
            .or_default();
        requirement.task_count =
            requirement
                .task_count
                .checked_add(1)
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: "completion requirement task count".to_string(),
                })?;
        if unsupported {
            requirement.unsupported_task_count = requirement
                .unsupported_task_count
                .checked_add(1)
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: "completion unsupported task count".to_string(),
                })?;
        }
    }
    let citation_count = u64::try_from(
        task.citations
            .iter()
            .filter(|citation| !citation.source_id.trim().is_empty())
            .count(),
    )
    .map_err(|_| Error::ArithmeticOverflow {
        context: "completion citation count".to_string(),
    })?;
    for check in &run.goal.completion_checks {
        let CompletionCheckKind::Citations {
            node_ids,
            min_per_task,
        } = &check.kind
        else {
            continue;
        };
        if node_ids.contains(&task.node_id) && citation_count < u64::from(*min_per_task) {
            let failed = evidence
                .citation_failures
                .entry(check.id.clone())
                .or_default();
            failed.failure_count =
                failed
                    .failure_count
                    .checked_add(1)
                    .ok_or_else(|| Error::ArithmeticOverflow {
                        context: "completion citation failure count".to_string(),
                    })?;
            if failed.samples.len() < MAX_EVIDENCE_SAMPLES_PER_CHECK {
                failed.samples.push(CitationFailureSample {
                    node_id: task.node_id.clone(),
                    item_key: task.item_key.clone(),
                    count: citation_count,
                });
            }
        }
    }
    Ok(())
}

async fn materialize_verifiers_in_tx(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
    evidence: &CompletionTaskEvidence,
    nodes: &CompletionNodeEvidence,
    limit: u32,
) -> Result<(Vec<ExecutionTaskRecord>, bool)> {
    let coverage_by_node = coverage_by_node(run, nodes, evidence)?;
    let unresolved = unsatisfied_requirements(run, nodes, &coverage_by_node);
    let terminal_output = nodes.terminal_output.clone();
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT node_id FROM moa.execution_task WHERE run_uid=$1 \
         AND node_id LIKE '@check/%' ORDER BY node_id LIMIT $2",
    )
    .bind(run.run_uid)
    .bind(
        i64::try_from(config.dispatch_batch_size)
            .map_err(|_| Error::ArithmeticOverflow {
                context: "completion verifier dispatch bound".to_string(),
            })?
            .checked_add(1)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "completion verifier dispatch bound".to_string(),
            })?,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if existing.len() > config.dispatch_batch_size {
        return Err(Error::InvalidRepositoryData {
            message: "persisted verifier tasks exceed the compiler-validated dispatch bound"
                .to_string(),
        });
    }
    let existing = existing.into_iter().collect::<BTreeSet<_>>();
    let declared = run
        .goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
        .map(|check| format!("@check/{}", check.id))
        .collect::<BTreeSet<_>>();
    if !existing.is_subset(&declared) {
        return Err(Error::InvalidRepositoryData {
            message: "persisted verifier task is not declared by the completion contract"
                .to_string(),
        });
    }
    let limit = usize::try_from(limit).map_err(|_| Error::ArithmeticOverflow {
        context: "completion verifier page limit".to_string(),
    })?;
    let mut tasks = Vec::new();
    let base_order = u64::try_from(run.active_plan.definition.nodes.len()).map_err(|_| {
        Error::ArithmeticOverflow {
            context: "completion verifier node order".to_string(),
        }
    })?;
    for (index, check) in run.goal.completion_checks.iter().enumerate() {
        let CompletionCheckKind::AgentVerifier {
            instructions,
            max_turns,
        } = &check.kind
        else {
            continue;
        };
        let node_id = format!("@check/{}", check.id);
        if existing.contains(&node_id) || tasks.len() >= limit {
            continue;
        }
        let item_key = format!("check:{}", check.id);
        tasks.push(LogicalTask {
            task_id: ExecutionTaskId::derive(run.run_uid, &node_id, &item_key)?,
            node_id: node_id.clone(),
            item_key,
            requirement_ids: unresolved.clone(),
            plan_revision: run.plan_revision,
            generation: 1,
            input: json!({
                "goal": &run.goal,
                "check_id": &check.id,
                "description": &check.description,
                "terminal_output": &terminal_output,
                "bounded_task_evidence": evidence,
            }),
            kind: LogicalTaskKind::CompletionVerifier {
                check_id: check.id.clone(),
                instructions: instructions.clone(),
                max_turns: *max_turns,
            },
            compensation: None,
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            reservation: verifier_turn_reservation(config, *max_turns)?,
        });
        let node_order = base_order
            .checked_add(u64::try_from(index).map_err(|_| Error::ArithmeticOverflow {
                context: "completion verifier node order".to_string(),
            })?)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "completion verifier node order".to_string(),
            })?;
        sqlx::query(
            "INSERT INTO moa.execution_node_state (node_state_uid,tenant_id,run_uid,node_id, \
                 node_order,node_status,materialization_cursor,materialization_complete, \
                 total_task_count,ready_task_count) \
             VALUES ($1,$2,$3,$4,$5,'ready',1,TRUE,1,1) \
             ON CONFLICT (run_uid,node_id) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(node_id)
        .bind(to_i64(node_order, "completion verifier node order")?)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    }
    let batch = prepare_task_materialization_batch(run.run_uid, run.plan_revision, &tasks)?;
    let inserted = sqlx::query(INSERT_TASK_BATCH_SQL)
        .bind(&batch)
        .bind(run.run_uid)
        .bind(run.tenant_id.0)
        .bind(run.contact_id.map(|contact| contact.0))
        .bind(to_i64(run.plan_revision, "completion plan revision")?)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    if inserted.len() != tasks.len() {
        return Err(Error::InvalidRepositoryData {
            message: "completion verifier materialization lost its exact task-key fence"
                .to_string(),
        });
    }
    let task_ids = tasks
        .iter()
        .map(|task| task.task_id.as_uuid())
        .collect::<Vec<_>>();
    sqlx::query(
        "UPDATE moa.execution_task SET status='ready', ready_at=NOW(), updated_at=NOW() \
         WHERE run_uid=$1 AND task_id=ANY($2::UUID[]) AND status='pending'",
    )
    .bind(run.run_uid)
    .bind(task_ids)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    sqlx::query(
        "UPDATE moa.execution_run SET progress_total_tasks=progress_total_tasks+$2, \
             ready_task_count=ready_task_count+$2, updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .bind(to_i64(
        u64::try_from(tasks.len()).map_err(|_| Error::ArithmeticOverflow {
            context: "completion verifier task count".to_string(),
        })?,
        "completion verifier task count",
    )?)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let rows = sqlx::query(LOAD_TASK_BATCH_SQL)
        .bind(&batch)
        .bind(run.run_uid)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    let records = rows.iter().map(task_from_row).collect::<Result<Vec<_>>>()?;
    let verifier_count = run
        .goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
        .count();
    let materialized_count =
        existing
            .len()
            .checked_add(records.len())
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "completion verifier materialized count".to_string(),
            })?;
    Ok((records, materialized_count == verifier_count))
}

async fn load_verifier_tasks(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
) -> Result<Vec<ExecutionTaskRecord>> {
    let declared = run
        .goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
        .map(|check| format!("@check/{}", check.id))
        .collect::<BTreeSet<_>>();
    let expected = declared.len();
    let fetch_limit = i64::try_from(expected)
        .map_err(|_| Error::ArithmeticOverflow {
            context: "completion verifier load bound".to_string(),
        })?
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "completion verifier load bound".to_string(),
        })?;
    let rows = sqlx::query(
        "SELECT * FROM moa.execution_task WHERE run_uid=$1 AND node_id LIKE '@check/%' \
         ORDER BY node_id,task_id LIMIT $2",
    )
    .bind(run.run_uid)
    .bind(fetch_limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let persisted = rows
        .iter()
        .map(|row| row.try_get::<String, _>("node_id").map_err(row_error))
        .collect::<Result<BTreeSet<_>>>()?;
    if rows.len() != expected || persisted != declared {
        return Err(Error::InvalidRepositoryData {
            message: "persisted verifier tasks do not exactly match declared completion checks"
                .to_string(),
        });
    }
    rows.iter().map(task_from_row).collect()
}

async fn load_existing_verifier_tasks(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
) -> Result<Vec<ExecutionTaskRecord>> {
    let declared = run
        .goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
        .map(|check| format!("@check/{}", check.id))
        .collect::<BTreeSet<_>>();
    let limit = i64::try_from(config.dispatch_batch_size)
        .map_err(|_| Error::ArithmeticOverflow {
            context: "replan-stop verifier load bound".to_string(),
        })?
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "replan-stop verifier load bound".to_string(),
        })?;
    let rows = sqlx::query(
        "SELECT * FROM moa.execution_task WHERE run_uid=$1 AND node_id LIKE '@check/%' \
         ORDER BY node_id,task_id LIMIT $2",
    )
    .bind(run.run_uid)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if rows.len() > config.dispatch_batch_size {
        return Err(Error::InvalidRepositoryData {
            message: "persisted verifier tasks exceed the compiler-validated dispatch bound"
                .to_string(),
        });
    }
    let persisted = rows
        .iter()
        .map(|row| row.try_get::<String, _>("node_id").map_err(row_error))
        .collect::<Result<BTreeSet<_>>>()?;
    if !persisted.is_subset(&declared) {
        return Err(Error::InvalidRepositoryData {
            message: "persisted verifier task is not declared by the completion contract"
                .to_string(),
        });
    }
    rows.iter().map(task_from_row).collect()
}

fn accumulate_node_evidence(
    run: &ExecutionRunRecord,
    row: &PgRow,
    evidence: &mut CompletionNodeEvidence,
    forced_failed_node_id: Option<&str>,
) -> Result<()> {
    let node_id: String = row.try_get("node_id").map_err(row_error)?;
    let status: String = row.try_get("node_status").map_err(row_error)?;
    let force_failed = forced_failed_node_id == Some(node_id.as_str());
    if !force_failed
        && !matches!(
            status.as_str(),
            "completed" | "skipped" | "failed" | "cancelled"
        )
    {
        return Err(Error::InvalidRepositoryData {
            message: format!("completion scan observed nonterminal node `{node_id}`"),
        });
    }
    let total = required_u64(row, "total_task_count")?;
    let succeeded = required_u64(row, "succeeded_task_count")?;
    let failed = required_u64(row, "failed_task_count")?;
    let cancelled = required_u64(row, "cancelled_task_count")?;
    let passed = !force_failed
        && status == "completed"
        && failed == 0
        && cancelled == 0
        && (total == 0 || succeeded > 0);
    let plan_node = run
        .active_plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!("completion node `{node_id}` is absent from the active plan"),
        })?;
    if force_failed || status != "skipped" {
        for requirement_id in &plan_node.requirement_ids {
            let requirement = evidence
                .requirements
                .entry(requirement_id.clone())
                .or_default();
            requirement.eligible_node_count = requirement
                .eligible_node_count
                .checked_add(1)
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: "completion eligible requirement node count".to_string(),
                })?;
            if passed {
                requirement.completed_node_count = requirement
                    .completed_node_count
                    .checked_add(1)
                    .ok_or_else(|| Error::ArithmeticOverflow {
                        context: "completion completed requirement node count".to_string(),
                    })?;
            }
        }
    }
    for check in &run.goal.completion_checks {
        let CompletionCheckKind::RequiredNodes { node_ids } = &check.kind else {
            continue;
        };
        if node_ids.contains(&node_id) {
            let check_evidence = evidence
                .required_checks
                .entry(check.id.clone())
                .or_default();
            check_evidence.observed_node_count = check_evidence
                .observed_node_count
                .checked_add(1)
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: "completion required-node observation count".to_string(),
                })?;
            if !passed {
                check_evidence.failed_node_ids.push(node_id.clone());
            }
        }
    }
    for coverage in run
        .goal
        .coverage
        .iter()
        .filter(|coverage| coverage.map_node_id == node_id)
    {
        let coverage_passed = passed && (coverage.require_all || total == 0 || succeeded > 0);
        evidence
            .coverage_passed
            .entry(coverage.id.clone())
            .and_modify(|current| *current &= coverage_passed)
            .or_insert(coverage_passed);
    }
    if matches!(plan_node.operation, ExecutionOperation::Output { .. }) {
        evidence.terminal_output = row.try_get("aggregate_output").map_err(row_error)?;
    }
    Ok(())
}

fn evaluate_persisted_completion(
    run: &ExecutionRunRecord,
    nodes: &CompletionNodeEvidence,
    evidence: &CompletionTaskEvidence,
    coverage_evaluations: &[PersistedCoverageEvaluation],
    verifier_tasks: &[ExecutionTaskRecord],
    now: DateTime<Utc>,
) -> Result<(CompletionEvaluation, Option<Value>)> {
    let terminal_output = nodes.terminal_output.clone();
    let mut checks = Vec::new();
    let mut coverage_by_node = BTreeMap::new();
    let mut failed_coverage = Vec::new();
    for coverage in &run.goal.coverage {
        let passed = coverage_evaluations
            .iter()
            .find(|evaluation| evaluation.coverage_id == coverage.id)
            .is_some_and(|evaluation| evaluation.passed);
        coverage_by_node
            .entry(coverage.map_node_id.clone())
            .and_modify(|node_passed| *node_passed &= passed)
            .or_insert(passed);
        if !passed {
            failed_coverage.push(coverage.id.clone());
        }
    }
    for check in &run.goal.completion_checks {
        let (passed, check_evidence) = match &check.kind {
            CompletionCheckKind::OutputSchema => {
                let passed = terminal_output.as_ref().is_some_and(|output| {
                    validate_instance(
                        &run.active_plan.definition.output_schema,
                        output,
                        "plan.output",
                    )
                    .is_ok()
                        && run
                            .active_plan
                            .definition
                            .nodes
                            .iter()
                            .find(|node| {
                                matches!(node.operation, ExecutionOperation::Output { .. })
                            })
                            .is_some_and(|node| {
                                validate_instance(&node.output_schema, output, "output_node.output")
                                    .is_ok()
                            })
                });
                (
                    passed,
                    json!({"terminal_output_present": terminal_output.is_some()}),
                )
            }
            CompletionCheckKind::RequiredNodes { node_ids } => {
                let persisted = nodes
                    .required_checks
                    .get(&check.id)
                    .cloned()
                    .unwrap_or_default();
                let incomplete = persisted.failed_node_ids;
                let observed = usize::try_from(persisted.observed_node_count).map_err(|_| {
                    Error::ArithmeticOverflow {
                        context: "completion required-node observation count".to_string(),
                    }
                })?;
                (
                    incomplete.is_empty() && observed == node_ids.len(),
                    json!({"incomplete_node_ids": incomplete}),
                )
            }
            CompletionCheckKind::MapCoverage { map_node_id } => {
                let matching = coverage_evaluations
                    .iter()
                    .filter(|coverage| coverage.map_node_id == *map_node_id)
                    .collect::<Vec<_>>();
                let passed = !matching.is_empty()
                    && coverage_by_node.get(map_node_id).copied().unwrap_or(false);
                (passed, serde_json::to_value(matching)?)
            }
            CompletionCheckKind::Citations { .. } => {
                let failed = evidence
                    .citation_failures
                    .get(&check.id)
                    .cloned()
                    .unwrap_or_default();
                (failed.failure_count == 0, serde_json::to_value(failed)?)
            }
            CompletionCheckKind::AgentVerifier { .. } => {
                let node_id = format!("@check/{}", check.id);
                let output = verifier_tasks
                    .iter()
                    .find(|task| {
                        task.node_id == node_id && task.status == ExecutionTaskStatus::Completed
                    })
                    .and_then(|task| task.output.as_ref());
                let object = output.and_then(Value::as_object);
                let valid = object.is_some_and(|object| {
                    object.len() == 2
                        && object.get("passed").and_then(Value::as_bool).is_some()
                        && object.contains_key("evidence")
                });
                let verdict = object
                    .and_then(|object| object.get("passed"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (
                    valid && verdict,
                    json!({"verdict": verdict, "valid_shape": valid, "evidence": object.and_then(|object| object.get("evidence")).cloned().unwrap_or(Value::Null)}),
                )
            }
        };
        checks.push(CompletionCheckResult {
            check_id: check.id.clone(),
            passed,
            evidence: check_evidence,
        });
    }
    let (satisfied, unsatisfied) = partition_requirements(run, nodes, &coverage_by_node);
    let mut gaps = checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| format!("completion check {} failed", check.check_id))
        .collect::<Vec<_>>();
    gaps.extend(
        failed_coverage
            .iter()
            .map(|id| format!("coverage {id} failed")),
    );
    gaps.extend(
        unsatisfied
            .iter()
            .map(|id| format!("requirement {id} is unsatisfied")),
    );
    let deliverables_pass = run.goal.deliverables.iter().all(|deliverable| {
        terminal_output
            .as_ref()
            .and_then(|output| output.pointer(&deliverable.output_pointer))
            .is_some_and(|value| {
                validate_instance(
                    &deliverable.schema,
                    value,
                    &format!("goal.deliverables.{}", deliverable.id),
                )
                .is_ok()
            })
    });
    if !deliverables_pass {
        gaps.push("one or more deliverables are missing or invalid".to_string());
    }
    let constraints_pass = run.goal.constraints.iter().all(|constraint| {
        run.goal
            .completion_checks
            .iter()
            .enumerate()
            .filter(|(_, check)| check.constraint_ids.contains(&constraint.id))
            .all(|(index, _)| checks.get(index).is_some_and(|result| result.passed))
    });
    if !constraints_pass {
        gaps.push("one or more constraint-linked checks failed".to_string());
    }
    let deadline_exceeded = run
        .approved_budget
        .deadline_at
        .is_some_and(|deadline| now > deadline);
    let limit_stop = if deadline_exceeded {
        Some(ExecutionLimitStop::DeadlineExceeded)
    } else if run.budget_overrun {
        Some(ExecutionLimitStop::BudgetExceeded)
    } else {
        None
    };
    if deadline_exceeded {
        gaps.push("execution deadline exceeded".to_string());
    }
    if run.budget_overrun {
        gaps.push("execution budget overrun".to_string());
    }
    gaps.sort();
    gaps.dedup();
    let all_pass = checks.iter().all(|check| check.passed)
        && failed_coverage.is_empty()
        && unsatisfied.is_empty()
        && deliverables_pass
        && constraints_pass
        && terminal_output.is_some();
    let useful = terminal_output.is_some() || !satisfied.is_empty();
    let fully_unsupported = unsatisfied.iter().any(|requirement_id| {
        evidence
            .unsupported_by_requirement
            .get(requirement_id)
            .is_some_and(|unsupported| {
                unsupported.task_count > 0
                    && unsupported.task_count == unsupported.unsupported_task_count
            })
    });
    let status = if all_pass && limit_stop.is_none() {
        CompletionStatus::Completed
    } else if evidence.authorization_denied {
        CompletionStatus::Blocked
    } else if fully_unsupported {
        CompletionStatus::Unsupported
    } else if useful {
        CompletionStatus::Partial
    } else {
        CompletionStatus::Failed
    };
    Ok((
        CompletionEvaluation {
            status,
            limit_stop,
            checks,
            satisfied_requirement_ids: satisfied,
            unsatisfied_requirement_ids: unsatisfied,
            gaps,
        },
        terminal_output,
    ))
}

fn partition_requirements(
    run: &ExecutionRunRecord,
    nodes: &CompletionNodeEvidence,
    coverage: &BTreeMap<String, bool>,
) -> (Vec<String>, Vec<String>) {
    let mut satisfied = Vec::new();
    let mut unsatisfied = Vec::new();
    for requirement in &run.goal.requirements {
        let requirement_nodes = nodes
            .requirements
            .get(&requirement.id)
            .cloned()
            .unwrap_or_default();
        let coverage_passed = run
            .active_plan
            .definition
            .nodes
            .iter()
            .filter(|node| {
                node.requirement_ids.contains(&requirement.id)
                    && matches!(node.operation, ExecutionOperation::Map { .. })
            })
            .all(|node| coverage.get(&node.id).copied().unwrap_or(true));
        let passed = requirement_nodes.eligible_node_count > 0
            && requirement_nodes.eligible_node_count == requirement_nodes.completed_node_count
            && coverage_passed;
        if passed {
            satisfied.push(requirement.id.clone());
        } else {
            unsatisfied.push(requirement.id.clone());
        }
    }
    satisfied.sort();
    unsatisfied.sort();
    (satisfied, unsatisfied)
}

fn unsatisfied_requirements(
    run: &ExecutionRunRecord,
    nodes: &CompletionNodeEvidence,
    coverage: &BTreeMap<String, bool>,
) -> Vec<String> {
    partition_requirements(run, nodes, coverage).1
}

fn validate_completion_runtime_bounds(
    run: &ExecutionRunRecord,
    config: &ExecutionConfig,
) -> Result<()> {
    let metadata_count = run
        .goal
        .requirements
        .len()
        .saturating_add(run.goal.constraints.len())
        .saturating_add(run.goal.deliverables.len())
        .saturating_add(run.goal.coverage.len())
        .saturating_add(run.goal.completion_checks.len());
    let referenced_nodes = run
        .goal
        .completion_checks
        .iter()
        .map(|check| match &check.kind {
            CompletionCheckKind::RequiredNodes { node_ids }
            | CompletionCheckKind::Citations { node_ids, .. } => node_ids.len(),
            CompletionCheckKind::MapCoverage { .. } => 1,
            CompletionCheckKind::OutputSchema | CompletionCheckKind::AgentVerifier { .. } => 0,
        })
        .fold(0_usize, usize::saturating_add);
    let verifier_count = run
        .goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
        .count();
    if metadata_count > config.maximum_activation_steps
        || referenced_nodes > config.maximum_activation_steps
        || verifier_count > config.dispatch_batch_size
    {
        return Err(Error::InvalidRepositoryData {
            message:
                "persisted completion contract exceeds its compiler-validated activation bounds"
                    .to_string(),
        });
    }
    Ok(())
}

/// Loads the earliest typed task failure so a run terminal keeps its real class.
///
/// Without this the run terminal reports a generic [`ExecutionFailureClass::Terminal`],
/// so a run that died on a rate limit, an unsupported capability, or an authorization
/// denial is indistinguishable from any other failure at the product surface. One
/// bounded lookup on the failing task restores the attribution.
async fn load_earliest_typed_task_failure(
    conn: &mut sqlx::PgConnection,
    run_uid: Uuid,
) -> Result<Option<ExecutionTaskFailure>> {
    let row = sqlx::query(
        "SELECT status,current_outcome \
         FROM moa.execution_task \
         WHERE run_uid = $1 \
           AND status IN ('failed', 'unknown_outcome') \
           AND current_outcome IS NOT NULL \
         ORDER BY completed_at NULLS LAST, task_id \
         LIMIT 1",
    )
    .bind(run_uid)
    .fetch_optional(conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let status: String = row.try_get("status").map_err(row_error)?;
    let outcome: Value = row.try_get("current_outcome").map_err(row_error)?;
    let outcome: ExecutionTaskOutcome =
        serde_json::from_value(outcome).map_err(|error| Error::InvalidRepositoryData {
            message: format!("terminal failure outcome is undecodable: {error}"),
        })?;
    let failure = match (status.as_str(), outcome.result) {
        ("failed", ExecutionTaskResult::Failed { class, message }) => ExecutionTaskFailure {
            class,
            message,
            capability_ref: None,
        },
        ("unknown_outcome", ExecutionTaskResult::UnknownOutcome { message }) => {
            ExecutionTaskFailure {
                class: ExecutionFailureClass::Terminal,
                message,
                capability_ref: None,
            }
        }
        (status, _) => {
            return Err(Error::InvalidRepositoryData {
                message: format!("terminal task status `{status}` has an incompatible outcome"),
            });
        }
    };
    Ok(Some(failure))
}

fn terminal_projection_for_evaluation(
    evaluation: &CompletionEvaluation,
    output: Option<Value>,
    typed_failure: Option<ExecutionTaskFailure>,
) -> Result<TerminalProjection> {
    crate::completion::terminal_projection_from_evaluation(
        evaluation,
        output,
        None,
        (evaluation.status == CompletionStatus::Failed).then(|| {
            // Prefer the failing task's own class over a flattened `Terminal`, but keep
            // the evaluation gaps as the message: they explain which requirements were
            // unmet, which the task-level message does not.
            typed_failure.map_or_else(
                || ExecutionTaskFailure {
                    class: ExecutionFailureClass::Terminal,
                    message: evaluation.gaps.join("; "),
                    capability_ref: None,
                },
                |failure| ExecutionTaskFailure {
                    class: failure.class,
                    message: evaluation.gaps.join("; "),
                    capability_ref: failure.capability_ref,
                },
            )
        }),
        (evaluation.status == CompletionStatus::Unsupported)
            .then(|| "required execution paths are unsupported".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{CompletionNodeEvidence, CompletionTaskEvidence};

    #[test]
    fn empty_persisted_completion_evidence_uses_the_canonical_defaults() {
        // Pins: a fresh completion scan persists `{}` for both evidence documents, and
        // the first controller page must decode them before accumulating any evidence.
        let task: CompletionTaskEvidence =
            serde_json::from_str("{}").expect("empty task evidence should decode");
        let node: CompletionNodeEvidence =
            serde_json::from_str("{}").expect("empty node evidence should decode");

        assert!(!task.authorization_denied);
        assert!(task.unsupported_by_requirement.is_empty());
        assert!(task.citation_failures.is_empty());
        assert!(node.terminal_output.is_none());
        assert!(node.requirements.is_empty());
        assert!(node.required_checks.is_empty());
        assert!(node.coverage_passed.is_empty());
    }
}
