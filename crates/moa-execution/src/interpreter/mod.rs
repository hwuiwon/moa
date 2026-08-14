//! Pure bounded logical-task materialization for execution-plan nodes.

mod catalog;
mod compensation;
mod materialize;
mod reservation;

use catalog::*;
pub use compensation::resolve_compensation_input;
use reservation::*;

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
    CapabilityReference, ExecutionFailureClass, ExecutionGoalContract, ExecutionNode,
    ExecutionOperation, ExecutionReducer, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionTemporalTarget, MapTask,
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
    },
    compiler::CanonicalExecutionPlan,
    schema::validate_instance,
    state::{ExecutionProjection, ExecutionTaskId, LogicalTask, LogicalTaskKind},
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

/// Complete pure input to one bounded materialization evaluation.
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
    /// Deterministic materialization time.
    pub now: DateTime<Utc>,
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
    /// Whether the node's declared condition evaluated false and the node must be skipped.
    pub condition_skipped: bool,
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
    // The condition is evaluated here, in the wrapper, so a false branch never enters
    // map or reduce paging at all: no items are resolved, no reduce round is opened,
    // and the node's whole source is declared exhausted in one empty page. It is
    // evaluated only at cursor zero because a condition that has already admitted its
    // first page must not be re-litigated mid-source; every input it can read is
    // immutable for the life of the node.
    if cursor == 0
        && let Some(condition) = &node.when
    {
        let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
        let context = BindingContext {
            run_input: &request.run_input,
            node_outputs: referenced_outputs,
            dependencies: &dependencies,
            item: None,
            item_key: None,
        };
        if !evaluate_condition(condition, &context)? {
            return Ok(NodeMaterializationPage {
                tasks: Vec::new(),
                next_cursor: 0,
                source_exhausted: true,
                reduce_cursor: None,
                terminal_output: None,
                condition_skipped: true,
            });
        }
    }
    materialize::materialize_node_page(request, node, referenced_outputs, cursor, limit, reduce)
}

/// Outcome of fencing one resolved temporal target against the run deadline.
///
/// A relative target is resolved against wait entry, not compile time, so a delay that was
/// legal when the plan compiled can land past the deadline by the time the wait is entered.
/// That is a normal product outcome for a long-horizon run, not an invalid plan, so it is
/// reported as a value rather than an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporalTargetResolution {
    /// The target resolves strictly before the run deadline.
    Due(DateTime<Utc>),
    /// The target resolves at or after the run deadline and cannot be waited on.
    DeadlineExceeded {
        /// Instant the wait would have become due.
        due_at: DateTime<Utc>,
        /// Absolute deadline of the owning run.
        run_deadline_at: DateTime<Utc>,
    },
}

/// Resolves an exact or wait-entry-relative temporal target and fences it by the run deadline.
///
/// Fails only on impossible input; a target past the run deadline is returned as
/// [`TemporalTargetResolution::DeadlineExceeded`] for the caller to project as a typed failure.
pub fn resolve_temporal_target_within_deadline(
    target: &ExecutionTemporalTarget,
    wait_entered_at: DateTime<Utc>,
    run_deadline_at: DateTime<Utc>,
) -> Result<TemporalTargetResolution> {
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
    Ok(if due_at >= run_deadline_at {
        TemporalTargetResolution::DeadlineExceeded {
            due_at,
            run_deadline_at,
        }
    } else {
        TemporalTargetResolution::Due(due_at)
    })
}

/// Resolves a temporal target that must already be strictly before the run deadline.
///
/// Callers settling an entered wait use this: the target was fenced at wait entry, so a
/// deadline violation here is a corrupted projection rather than a product outcome.
pub fn resolve_temporal_target(
    target: &ExecutionTemporalTarget,
    wait_entered_at: DateTime<Utc>,
    run_deadline_at: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    match resolve_temporal_target_within_deadline(target, wait_entered_at, run_deadline_at)? {
        TemporalTargetResolution::Due(due_at) => Ok(due_at),
        TemporalTargetResolution::DeadlineExceeded { .. } => Err(Error::InvalidProjection {
            message: "temporal target must be earlier than the run deadline".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests;
