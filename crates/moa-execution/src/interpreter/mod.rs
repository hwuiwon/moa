//! Pure execution-plan scheduling and logical-task materialization.

mod aggregate;
mod compensation;
mod materialize;
mod projection;
mod reservation;
mod temporal_wait;
mod terminal;

use aggregate::*;
pub use compensation::resolve_compensation_input;
use materialize::*;
use projection::*;
use reservation::*;
use temporal_wait::*;
use terminal::*;

/// Derives the bounded reservation for one persisted completion verifier.
pub(crate) fn verifier_turn_reservation(
    config: &ExecutionConfig,
    max_turns: u32,
) -> Result<ExecutionEstimate> {
    turn_reservation(config, max_turns, 1, true)
}

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheckKind, ExecutionFailureClass, ExecutionGoalContract,
    ExecutionNode, ExecutionOperation, ExecutionReducer, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionTemporalTarget, MapTask, RetryPolicy,
};

use moa_config::ExecutionConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    Error, Result,
    bindings::{BindingContext, evaluate_condition, extract_map_key, resolve_bindings},
    budget::BudgetLedger,
    capability::{
        ExecutionCapability, ExecutionCapabilityCatalog, ExecutionEstimate, canonical_sort_key,
        catalog_hash, task_output_hash,
    },
    compiler::CanonicalExecutionPlan,
    completion::{
        CompletionEvaluationRequest, CompletionStatus, completed_output, evaluate_completion,
        map_output, node_outputs, terminal_projection_from_evaluation,
    },
    schema::validate_instance,
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionTaskFailure, ExecutionTaskId,
        ExecutionTaskStatus, LogicalTask, LogicalTaskKind, ScheduleDecision, TerminalProjection,
        VerifierTaskSummary, WaitingReason, task_status_from_outcome,
    },
};

/// Validates a completed task outcome against its concrete plan-node output contract.
#[must_use]
pub fn validate_task_outcome(
    plan: &CanonicalExecutionPlan,
    node_id: &str,
    kind: &LogicalTaskKind,
    mut outcome: ExecutionTaskOutcome,
) -> ExecutionTaskOutcome {
    let ExecutionTaskResult::Completed { output, .. } = &outcome.result else {
        return outcome;
    };
    let validation = match kind {
        LogicalTaskKind::CompletionVerifier { .. } => {
            let valid = output.as_object().is_some_and(|object| {
                object.len() == 2
                    && object.get("passed").and_then(Value::as_bool).is_some()
                    && object.contains_key("evidence")
            });
            valid.then_some(()).ok_or_else(|| {
                "completion verifier output must contain exactly boolean `passed` and `evidence`"
                    .to_string()
            })
        }
        _ => plan
            .definition
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| format!("active plan has no node `{node_id}`"))
            .and_then(|node| {
                validate_instance(
                    task_output_schema(node),
                    output,
                    "execution_task.node_output",
                )
                .map_err(|error| error.to_string())
            }),
    };
    if let Err(message) = validation {
        outcome.result = ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::InvalidOutput,
            message,
        };
    }
    outcome
}

fn task_output_schema(node: &ExecutionNode) -> &Value {
    match &node.operation {
        ExecutionOperation::Map {
            item_output_schema, ..
        } => item_output_schema,
        ExecutionOperation::Capability { .. }
        | ExecutionOperation::Agent { .. }
        | ExecutionOperation::Reduce { .. }
        | ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::WaitUntil { .. }
        | ExecutionOperation::Output { .. } => &node.output_schema,
    }
}

/// Complete pure input to one scheduler evaluation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    /// Stable execution-run UUID used in logical task IDs.
    pub run_uid: Uuid,
    /// Immutable execution goal contract.
    pub goal: ExecutionGoalContract,
    /// Active canonical execution plan.
    pub plan: CanonicalExecutionPlan,
    /// Immutable capability catalog snapshot pinned by the canonical plan hash.
    pub catalog: ExecutionCapabilityCatalog,
    /// Immutable run input.
    pub run_input: Value,
    /// Current durable node/task projection.
    pub projection: ExecutionProjection,
    /// Execution and verifier turn estimates.
    pub config: ExecutionConfig,
    /// Current pure run-level budget ledger.
    pub budget_ledger: BudgetLedger,
    /// Deterministic scheduler time.
    pub now: DateTime<Utc>,
}

/// One scheduler decision paired with the exact effective projection it evaluated.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleOutcome {
    /// Ready, waiting, terminal, or no-progress scheduler decision.
    pub decision: ScheduleDecision,
    /// Projection after deterministic conditions and aggregate nodes were derived.
    pub effective_projection: ExecutionProjection,
}

/// One bounded deterministic logical-task page for a single eligible node.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeMaterializationPage {
    /// Stable logical tasks for this source cursor page.
    pub tasks: Vec<LogicalTask>,
    /// Cursor immediately after this page.
    pub next_cursor: u64,
    /// Whether the deterministic materialization source is exhausted.
    pub source_exhausted: bool,
    /// Exact reduce-round source fence used for this page, when this is a reduce node.
    pub reduce_cursor: Option<ReduceMaterializationCursor>,
    /// Aggregate output for a source that completes without creating a logical task.
    pub terminal_output: Option<Value>,
}

/// Exact reduce-round source position used to derive one materialization page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReduceMaterializationCursor {
    /// One-based reduction round.
    pub round: u64,
    /// Number of batches committed before this page.
    pub batch_cursor: u64,
    /// Total input values consumed by this round.
    pub round_input_count: u64,
}

/// Bounded source slice and persisted cursor for one reduce round.
#[derive(Clone, Debug, PartialEq)]
pub struct ReduceMaterializationPageInput {
    /// One-based reduction round.
    pub round: u64,
    /// Number of batches already materialized in this round.
    pub batch_cursor: u64,
    /// Persisted input count, or `None` only while round one resolves immutable plan items.
    pub round_input_count: Option<u64>,
    /// Exact contiguous input values for this page; empty in round one, whose source is the plan.
    pub page_inputs: Vec<Value>,
}

/// Materializes only one bounded eligible-node page without a full task projection.
pub fn materialize_node_page(
    request: &ScheduleRequest,
    node_id: &str,
    referenced_outputs: &BTreeMap<String, Value>,
    cursor: u64,
    limit: u32,
    reduce: Option<&ReduceMaterializationPageInput>,
) -> Result<NodeMaterializationPage> {
    if limit == 0 || limit > 1_000 {
        return Err(Error::InvalidProjection {
            message: "node materialization page limit must be 1..=1000".to_string(),
        });
    }
    validate_scheduler_catalog(&request.catalog)?;
    let node = request
        .plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| Error::InvalidProjection {
            message: format!("active plan has no node `{node_id}`"),
        })?;
    if node
        .depends_on
        .iter()
        .any(|dependency| !referenced_outputs.contains_key(dependency))
    {
        return Err(Error::InvalidProjection {
            message: format!("node `{node_id}` is missing a direct dependency output"),
        });
    }
    materialize::materialize_node_page(request, node, referenced_outputs, cursor, limit, reduce)
}

/// Resolves an exact or wait-entry-relative temporal target and fences it by the run deadline.
pub fn resolve_temporal_target(
    target: &ExecutionTemporalTarget,
    wait_entered_at: DateTime<Utc>,
    run_deadline_at: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let due_at = match target {
        ExecutionTemporalTarget::At { at } => *at,
        ExecutionTemporalTarget::After { delay_seconds } => {
            if *delay_seconds == 0 {
                return Err(Error::InvalidProjection {
                    message: "relative temporal delay must be greater than zero".to_string(),
                });
            }
            let seconds = i64::try_from(*delay_seconds).map_err(|_| Error::InvalidProjection {
                message: "relative temporal delay exceeds supported timestamp range".to_string(),
            })?;
            let delay = chrono::TimeDelta::try_seconds(seconds).ok_or_else(|| {
                Error::InvalidProjection {
                    message: "relative temporal delay exceeds supported timestamp range"
                        .to_string(),
                }
            })?;
            wait_entered_at
                .checked_add_signed(delay)
                .ok_or_else(|| Error::InvalidProjection {
                    message: "relative temporal target overflows supported timestamp range"
                        .to_string(),
                })?
        }
    };
    if due_at >= run_deadline_at {
        return Err(Error::InvalidProjection {
            message: "temporal target must be earlier than the run deadline".to_string(),
        });
    }
    Ok(due_at)
}

/// Returns map nodes whose first deterministic materialization contains zero items.
///
/// The repository uses these node IDs to persist a zero-fan-out marker even though
/// the scheduler has no logical task row to return for an empty map.
pub fn ready_empty_map_nodes(request: &ScheduleRequest) -> Result<Vec<String>> {
    validate_projection(request)?;
    let mut outputs = node_outputs(&request.plan, &request.projection)?;
    let mut statuses = request.projection.node_statuses.clone();
    apply_false_conditions(request, &mut statuses, &mut outputs)?;
    derive_aggregate_nodes(request, &mut statuses, &mut outputs)?;
    apply_false_conditions(request, &mut statuses, &mut outputs)?;

    let mut node_ids = Vec::new();
    for node in &request.plan.definition.nodes {
        let ExecutionOperation::Map {
            items, max_items, ..
        } = &node.operation
        else {
            continue;
        };
        if effective_status(&statuses, &node.id) != Some(ExecutionNodeStatus::Completed)
            || request
                .projection
                .tasks
                .iter()
                .any(|task| task.node_id == node.id)
        {
            continue;
        }
        let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
        let resolved = resolve_bindings(
            items,
            &BindingContext {
                run_input: &request.run_input,
                node_outputs: &outputs,
                dependencies: &dependencies,
                item: None,
                item_key: None,
            },
        )?;
        let values = resolved.as_array().ok_or_else(|| Error::Binding {
            path: format!("node.{}.operation.items", node.id),
            message: "map items must resolve to an array".to_string(),
        })?;
        let count = u64::try_from(values.len()).map_err(|_| Error::ArithmeticOverflow {
            context: format!("map {} item count", node.id),
        })?;
        if count > *max_items {
            return Err(Error::InvalidProjection {
                message: format!("map {} exceeds max_items", node.id),
            });
        }
        if values.is_empty() {
            node_ids.push(node.id.clone());
        }
    }
    node_ids.sort();
    Ok(node_ids)
}

/// Returns one decision together with the exact effective projection it evaluated.
pub fn schedule(mut request: ScheduleRequest) -> Result<ScheduleOutcome> {
    validate_projection(&request)?;
    let mut outputs = node_outputs(&request.plan, &request.projection)?;
    let mut statuses = request.projection.node_statuses.clone();
    apply_false_conditions(&request, &mut statuses, &mut outputs)?;
    derive_aggregate_nodes(&request, &mut statuses, &mut outputs)?;
    apply_false_conditions(&request, &mut statuses, &mut outputs)?;
    request.projection.node_statuses = statuses.clone();

    let ordinary_terminal = request
        .plan
        .definition
        .nodes
        .iter()
        .all(|node| effective_status(&statuses, &node.id).is_some_and(is_terminal_node_status));
    if ordinary_terminal {
        let effective_projection = request.projection.clone();
        return schedule_verifiers_or_complete(request).map(|decision| ScheduleOutcome {
            decision,
            effective_projection,
        });
    }

    if request
        .budget_ledger
        .limit
        .deadline_at
        .is_some_and(|deadline| request.now >= deadline)
    {
        let decision = completion_terminal(
            &request,
            terminal_output(&request.plan, &request.projection),
        )?;
        return Ok(ScheduleOutcome {
            decision,
            effective_projection: request.projection,
        });
    }

    if let Some(settlement) = ready_wait_settlement(&request, &outputs)? {
        return Ok(ScheduleOutcome {
            decision: ScheduleDecision::SettleWait(settlement),
            effective_projection: request.projection,
        });
    }

    let mut ready = Vec::new();
    let mut dependency_waits = BTreeSet::new();
    for node in &request.plan.definition.nodes {
        if effective_status(&statuses, &node.id) != Some(ExecutionNodeStatus::Pending) {
            continue;
        }
        let dependency_statuses = node
            .depends_on
            .iter()
            .map(|id| (id, effective_status(&statuses, id)))
            .collect::<Vec<_>>();
        if dependency_statuses.iter().any(|(_, status)| {
            matches!(
                status,
                Some(ExecutionNodeStatus::Failed | ExecutionNodeStatus::Cancelled)
            )
        }) {
            return Ok(ScheduleOutcome {
                decision: ScheduleDecision::Terminal(TerminalProjection::Failed {
                    failure: ExecutionTaskFailure {
                        class: ExecutionFailureClass::DependencyFailed,
                        message: format!("node {} has a terminal failed dependency", node.id),
                        capability_ref: operation_capability(&node.operation),
                    },
                }),
                effective_projection: request.projection,
            });
        }
        if !dependency_statuses.iter().all(|(_, status)| {
            matches!(
                status,
                Some(ExecutionNodeStatus::Completed | ExecutionNodeStatus::Skipped)
            )
        }) {
            dependency_waits.insert(node.id.clone());
            continue;
        }

        let mut materialized = materialize_node(&request, node, &outputs)?;
        ready.append(&mut materialized);
    }

    if !ready.is_empty() {
        ready.sort_by(|left, right| {
            (&left.node_id, &left.item_key, left.task_id).cmp(&(
                &right.node_id,
                &right.item_key,
                right.task_id,
            ))
        });
        let mut ledger = request.budget_ledger.clone();
        for task in &ready {
            if ledger.try_reserve(task.reservation).is_err() {
                return Ok(ScheduleOutcome {
                    decision: budget_terminal(terminal_output(&request.plan, &request.projection)),
                    effective_projection: request.projection,
                });
            }
        }
        return Ok(ScheduleOutcome {
            decision: ScheduleDecision::Ready(ready),
            effective_projection: request.projection,
        });
    }

    let waiting = waiting_reasons(&request, dependency_waits);
    if !waiting.is_empty() {
        return Ok(ScheduleOutcome {
            decision: ScheduleDecision::Waiting(waiting),
            effective_projection: request.projection,
        });
    }

    let mut pending_node_ids = request
        .plan
        .definition
        .nodes
        .iter()
        .filter(|node| !effective_status(&statuses, &node.id).is_some_and(is_terminal_node_status))
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    pending_node_ids.sort();
    pending_node_ids.dedup();
    Ok(ScheduleOutcome {
        decision: ScheduleDecision::NoProgress { pending_node_ids },
        effective_projection: request.projection,
    })
}

#[cfg(test)]
mod tests;
