//! Pure execution-plan scheduling and logical-task materialization.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheckKind, ExecutionFailureClass, ExecutionGoalContract,
    ExecutionNode, ExecutionOperation, ExecutionReducer, ExecutionTaskResult, MapTask, RetryPolicy,
};
use moa_core::config::ExecutionConfig;
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
        map_output, node_outputs,
    },
    schema::validate_instance,
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionTaskFailure, ExecutionTaskId,
        ExecutionTaskStatus, LogicalTask, LogicalTaskKind, ScheduleDecision, TerminalProjection,
        VerifierTaskSummary, WaitingReason, task_status_from_outcome,
    },
};

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
        .is_some_and(|deadline| request.now > deadline)
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

fn validate_projection(request: &ScheduleRequest) -> Result<()> {
    validate_scheduler_catalog(&request.catalog)?;
    let canonical_catalog_hash = catalog_hash(
        request.catalog.schema_version,
        &request.catalog.capabilities,
    )?;
    if canonical_catalog_hash != request.plan.catalog_hash
        || request.catalog.catalog_hash != canonical_catalog_hash
    {
        return Err(Error::InvalidProjection {
            message: "scheduler capability catalog hash does not match the canonical plan"
                .to_string(),
        });
    }

    for task in &request.projection.tasks {
        if task.attempt == 0 || task.generation == 0 {
            return Err(Error::InvalidProjection {
                message: format!("task {} has a zero attempt or generation", task.task_id),
            });
        }
        let expected = ExecutionTaskId::derive(request.run_uid, &task.node_id, &task.item_key)?;
        if task.task_id != expected {
            return Err(Error::InvalidProjection {
                message: format!(
                    "task {} does not match its framed logical identity",
                    task.task_id
                ),
            });
        }
        if let Some(outcome) = &task.outcome {
            if outcome.schema_version != 1 {
                return Err(Error::InvalidProjection {
                    message: format!("task {} outcome schema_version must equal 1", task.task_id),
                });
            }
            let expected_status =
                task_status_from_outcome(outcome, task.status == ExecutionTaskStatus::Running);
            if task.status != expected_status {
                return Err(Error::InvalidProjection {
                    message: format!(
                        "task {} status does not match its persisted outcome",
                        task.task_id
                    ),
                });
            }
        } else if matches!(
            task.status,
            ExecutionTaskStatus::Completed
                | ExecutionTaskStatus::Failed
                | ExecutionTaskStatus::Cancelled
                | ExecutionTaskStatus::WaitingInput
                | ExecutionTaskStatus::WaitingReplan
        ) {
            return Err(Error::InvalidProjection {
                message: format!(
                    "task {} terminal/waiting status has no outcome",
                    task.task_id
                ),
            });
        }

        let Some(node) = request
            .plan
            .definition
            .nodes
            .iter()
            .find(|node| node.id == task.node_id)
        else {
            if task.node_id.starts_with("@check/") {
                continue;
            }
            return Err(Error::InvalidProjection {
                message: format!("task {} references an unknown plan node", task.task_id),
            });
        };
        if task.attempt > node.retry.max_attempts {
            return Err(Error::InvalidProjection {
                message: format!("task {} exceeds its retry policy", task.task_id),
            });
        }
        if let Some(reference) = operation_capability(&node.operation) {
            let capability = find_capability(&request.catalog, &reference)?;
            validate_instance(
                &capability.input_schema,
                &task.input,
                &format!("task.{}.input", task.task_id),
            )?;
            if let Some(output) = completed_output(task) {
                validate_instance(
                    &capability.output_schema,
                    &output,
                    &format!("task.{}.output", task.task_id),
                )?;
            }
        }
    }
    Ok(())
}

fn validate_scheduler_catalog(catalog: &ExecutionCapabilityCatalog) -> Result<()> {
    if catalog.schema_version != 1 {
        return Err(Error::InvalidProjection {
            message: "scheduler capability catalog schema_version must equal 1".to_string(),
        });
    }
    let mut previous = None;
    for capability in &catalog.capabilities {
        if capability.estimate.tasks != 1 {
            return Err(Error::InvalidProjection {
                message: format!(
                    "capability {}@{} must reserve exactly one logical task",
                    capability.reference.name, capability.reference.version
                ),
            });
        }
        let key = canonical_sort_key(&capability.reference)?;
        if previous.as_ref().is_some_and(|previous| key <= *previous) {
            return Err(Error::InvalidProjection {
                message: "scheduler capability catalog must be sorted and duplicate-free"
                    .to_string(),
            });
        }
        previous = Some(key);
    }
    Ok(())
}

fn apply_false_conditions(
    request: &ScheduleRequest,
    statuses: &mut BTreeMap<String, ExecutionNodeStatus>,
    outputs: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let mut changed = true;
    while changed {
        changed = false;
        for node in &request.plan.definition.nodes {
            if effective_status(statuses, &node.id) != Some(ExecutionNodeStatus::Pending)
                || node.when.is_none()
                || !node.depends_on.iter().all(|id| {
                    matches!(
                        effective_status(statuses, id),
                        Some(ExecutionNodeStatus::Completed | ExecutionNodeStatus::Skipped)
                    )
                })
            {
                continue;
            }
            let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
            let context = BindingContext {
                run_input: &request.run_input,
                node_outputs: outputs,
                dependencies: &dependencies,
                item: None,
                item_key: None,
            };
            if let Some(condition) = &node.when
                && !evaluate_condition(condition, &context)?
            {
                statuses.insert(node.id.clone(), ExecutionNodeStatus::Skipped);
                outputs.insert(node.id.clone(), Value::Null);
                changed = true;
            }
        }
    }
    Ok(())
}

fn derive_aggregate_nodes(
    request: &ScheduleRequest,
    statuses: &mut BTreeMap<String, ExecutionNodeStatus>,
    outputs: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let mut changed = true;
    while changed {
        changed = false;
        for node in &request.plan.definition.nodes {
            if matches!(
                effective_status(statuses, &node.id),
                Some(
                    ExecutionNodeStatus::Skipped
                        | ExecutionNodeStatus::Failed
                        | ExecutionNodeStatus::Cancelled
                )
            ) || !node.depends_on.iter().all(|dependency| {
                matches!(
                    effective_status(statuses, dependency),
                    Some(ExecutionNodeStatus::Completed | ExecutionNodeStatus::Skipped)
                )
            }) {
                continue;
            }

            let aggregate = match &node.operation {
                ExecutionOperation::Map {
                    items,
                    item_key,
                    max_items,
                    ..
                } => derive_map_output(MapDerivationRequest {
                    schedule: request,
                    node,
                    outputs,
                    items,
                    item_key_pointer: item_key,
                    max_items: *max_items,
                })?,
                ExecutionOperation::Reduce {
                    items,
                    max_items,
                    batch_size,
                    ..
                } => derive_reduce_output(ReduceDerivationRequest {
                    schedule: request,
                    node,
                    outputs,
                    items,
                    max_items: *max_items,
                    batch_size: *batch_size,
                })?,
                ExecutionOperation::Capability { .. }
                | ExecutionOperation::Agent { .. }
                | ExecutionOperation::Review { .. }
                | ExecutionOperation::WaitSignal { .. }
                | ExecutionOperation::Output { .. } => AggregateState::Pending,
            };

            match aggregate {
                AggregateState::Pending => {
                    if matches!(
                        node.operation,
                        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. }
                    ) && effective_status(statuses, &node.id)
                        == Some(ExecutionNodeStatus::Completed)
                    {
                        return Err(Error::InvalidProjection {
                            message: format!(
                                "aggregate node {} is completed before all deterministic work is terminal",
                                node.id
                            ),
                        });
                    }
                }
                AggregateState::Completed(output) => {
                    if outputs.get(&node.id) != Some(&output)
                        || effective_status(statuses, &node.id)
                            != Some(ExecutionNodeStatus::Completed)
                    {
                        outputs.insert(node.id.clone(), output);
                        statuses.insert(node.id.clone(), ExecutionNodeStatus::Completed);
                        changed = true;
                    }
                }
                AggregateState::Failed => {
                    statuses.insert(node.id.clone(), ExecutionNodeStatus::Failed);
                    changed = true;
                }
                AggregateState::Cancelled => {
                    statuses.insert(node.id.clone(), ExecutionNodeStatus::Cancelled);
                    changed = true;
                }
            }
        }
    }
    Ok(())
}

enum AggregateState {
    Pending,
    Completed(Value),
    Failed,
    Cancelled,
}

struct MapDerivationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    item_key_pointer: &'a str,
    max_items: u64,
}

fn derive_map_output(request: MapDerivationRequest<'_>) -> Result<AggregateState> {
    let MapDerivationRequest {
        schedule,
        node,
        outputs,
        items,
        item_key_pointer,
        max_items,
    } = request;
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let resolved = resolve_bindings(
        items,
        &BindingContext {
            run_input: &schedule.run_input,
            node_outputs: outputs,
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
    if count > max_items {
        return Err(Error::InvalidProjection {
            message: format!("map {} exceeds max_items", node.id),
        });
    }

    let mut expected = BTreeSet::new();
    for item in values {
        let key = extract_map_key(item, item_key_pointer)?;
        if !expected.insert(key) {
            return Err(Error::InvalidProjection {
                message: format!("map {} produced duplicate item keys", node.id),
            });
        }
    }
    let tasks = schedule
        .projection
        .tasks
        .iter()
        .filter(|task| task.node_id == node.id)
        .collect::<Vec<_>>();
    if tasks.iter().any(|task| !expected.contains(&task.item_key)) {
        return Err(Error::InvalidProjection {
            message: format!("map {} projection contains an unexpected item key", node.id),
        });
    }
    if expected.iter().any(|key| {
        !tasks
            .iter()
            .any(|task| task.item_key == *key && is_terminal_task_status(task.status))
    }) {
        return Ok(AggregateState::Pending);
    }

    let aggregate = serde_json::to_value(map_output(node, &schedule.projection)?)?;
    validate_instance(
        &node.output_schema,
        &aggregate,
        &format!("node.{}.output", node.id),
    )?;
    Ok(AggregateState::Completed(aggregate))
}

struct ReduceDerivationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    max_items: u64,
    batch_size: u32,
}

fn derive_reduce_output(request: ReduceDerivationRequest<'_>) -> Result<AggregateState> {
    let ReduceDerivationRequest {
        schedule,
        node,
        outputs,
        items,
        max_items,
        batch_size,
    } = request;
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let resolved = resolve_bindings(
        items,
        &BindingContext {
            run_input: &schedule.run_input,
            node_outputs: outputs,
            dependencies: &dependencies,
            item: None,
            item_key: None,
        },
    )?;
    let round_items = resolved.as_array().cloned().ok_or_else(|| Error::Binding {
        path: format!("node.{}.operation.items", node.id),
        message: "reduce items must resolve to an array".to_string(),
    })?;
    let count = u64::try_from(round_items.len()).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} item count", node.id),
    })?;
    if count > max_items {
        return Err(Error::InvalidProjection {
            message: format!("reduce {} exceeds max_items", node.id),
        });
    }
    let Some(single) = round_items.first().filter(|_| round_items.len() == 1) else {
        if round_items.is_empty() {
            return Err(Error::InvalidProjection {
                message: format!("reduce {} requires at least one item", node.id),
            });
        }
        return derive_reduce_rounds(schedule, node, round_items, batch_size);
    };
    validate_instance(
        &node.output_schema,
        single,
        &format!("node.{}.output", node.id),
    )?;
    Ok(AggregateState::Completed(single.clone()))
}

fn derive_reduce_rounds(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    mut round_items: Vec<Value>,
    batch_size: u32,
) -> Result<AggregateState> {
    let batch_size = usize::try_from(batch_size).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} batch_size", node.id),
    })?;
    let mut round = 1_u32;
    loop {
        let mut completed = Vec::new();
        for (batch_index, _) in round_items.chunks(batch_size).enumerate() {
            let batch_index =
                u64::try_from(batch_index).map_err(|_| Error::ArithmeticOverflow {
                    context: format!("reduce {} batch index", node.id),
                })?;
            let item_key = format!("r{round}:b{batch_index}");
            let Some(task) = request
                .projection
                .tasks
                .iter()
                .find(|task| task.node_id == node.id && task.item_key == item_key)
            else {
                return Ok(AggregateState::Pending);
            };
            match task.status {
                ExecutionTaskStatus::Completed => {
                    let output =
                        completed_output(task).ok_or_else(|| Error::InvalidProjection {
                            message: format!(
                                "completed reducer task {} has no output",
                                task.task_id
                            ),
                        })?;
                    validate_instance(
                        &node.output_schema,
                        &output,
                        &format!("node.{}.reduce.{item_key}", node.id),
                    )?;
                    completed.push(output);
                }
                ExecutionTaskStatus::Failed => return Ok(AggregateState::Failed),
                ExecutionTaskStatus::Cancelled => return Ok(AggregateState::Cancelled),
                ExecutionTaskStatus::Skipped => {
                    return Err(Error::InvalidProjection {
                        message: format!("reducer task {} cannot be skipped", task.task_id),
                    });
                }
                ExecutionTaskStatus::Pending
                | ExecutionTaskStatus::Reserved
                | ExecutionTaskStatus::Running
                | ExecutionTaskStatus::WaitingInput
                | ExecutionTaskStatus::WaitingReplan => return Ok(AggregateState::Pending),
            }
        }
        if completed.len() == 1 {
            return Ok(AggregateState::Completed(completed.remove(0)));
        }
        round_items = completed;
        round = round
            .checked_add(1)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: format!("reduce {} round", node.id),
            })?;
    }
}

fn materialize_node(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    outputs: &BTreeMap<String, Value>,
) -> Result<Vec<LogicalTask>> {
    match &node.operation {
        ExecutionOperation::Map {
            items,
            item_key,
            max_items,
            task,
            ..
        } => materialize_map(MapMaterializationRequest {
            schedule: request,
            node,
            outputs,
            items,
            item_key_pointer: item_key,
            max_items: *max_items,
            task,
        }),
        ExecutionOperation::Reduce {
            items,
            max_items,
            reducer,
            batch_size,
        } => materialize_reduce(ReduceMaterializationRequest {
            schedule: request,
            node,
            outputs,
            items,
            max_items: *max_items,
            reducer,
            batch_size: *batch_size,
        }),
        ExecutionOperation::Capability { .. }
        | ExecutionOperation::Agent { .. }
        | ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => {
            if request
                .projection
                .tasks
                .iter()
                .any(|task| task.node_id == node.id && task.item_key.is_empty())
            {
                return Ok(Vec::new());
            }
            let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
            let context = BindingContext {
                run_input: &request.run_input,
                node_outputs: outputs,
                dependencies: &dependencies,
                item: None,
                item_key: None,
            };
            let input = resolve_bindings(&node.input, &context)?;
            let kind = logical_kind(&node.operation, &context)?;
            validate_capability_input(request, &node.operation, &input)?;
            if let LogicalTaskKind::Output { value } = &kind {
                validate_instance(
                    &node.output_schema,
                    value,
                    &format!("node.{}.output", node.id),
                )?;
                validate_instance(&request.plan.definition.output_schema, value, "plan.output")?;
            }
            let reservation =
                operation_reservation(request, &node.operation, node.retry.max_attempts)?;
            Ok(vec![logical_task(
                request,
                node,
                String::new(),
                input,
                kind,
                reservation,
            )?])
        }
    }
}

struct MapMaterializationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    item_key_pointer: &'a str,
    max_items: u64,
    task: &'a MapTask,
}

fn materialize_map(request: MapMaterializationRequest<'_>) -> Result<Vec<LogicalTask>> {
    let MapMaterializationRequest {
        schedule,
        node,
        outputs,
        items,
        item_key_pointer,
        max_items,
        task,
    } = request;
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let base = BindingContext {
        run_input: &schedule.run_input,
        node_outputs: outputs,
        dependencies: &dependencies,
        item: None,
        item_key: None,
    };
    let resolved = resolve_bindings(items, &base)?;
    let values = resolved.as_array().ok_or_else(|| Error::Binding {
        path: format!("node.{}.operation.items", node.id),
        message: "map items must resolve to an array".to_string(),
    })?;
    let count = u64::try_from(values.len()).map_err(|_| Error::ArithmeticOverflow {
        context: format!("map {} item count", node.id),
    })?;
    if count > max_items {
        return Err(Error::InvalidProjection {
            message: format!("map {} exceeds max_items", node.id),
        });
    }
    let mut seen = BTreeSet::new();
    let mut ready = Vec::new();
    for item in values {
        let item_key = extract_map_key(item, item_key_pointer)?;
        if !seen.insert(item_key.clone()) {
            return Err(Error::InvalidProjection {
                message: format!("map {} produced duplicate item key {item_key}", node.id),
            });
        }
        if schedule
            .projection
            .tasks
            .iter()
            .any(|existing| existing.node_id == node.id && existing.item_key == item_key)
        {
            continue;
        }
        let context = BindingContext {
            item: Some(item),
            item_key: Some(&item_key),
            ..base
        };
        let input = resolve_bindings(&node.input, &context)?;
        let kind = map_kind(task);
        validate_map_capability_input(schedule, task, &input)?;
        let reservation = map_task_reservation(schedule, task, node.retry.max_attempts)?;
        ready.push(logical_task(
            schedule,
            node,
            item_key,
            input,
            kind,
            reservation,
        )?);
    }
    Ok(ready)
}

struct ReduceMaterializationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    max_items: u64,
    reducer: &'a ExecutionReducer,
    batch_size: u32,
}

fn materialize_reduce(request: ReduceMaterializationRequest<'_>) -> Result<Vec<LogicalTask>> {
    let ReduceMaterializationRequest {
        schedule,
        node,
        outputs,
        items,
        max_items,
        reducer,
        batch_size,
    } = request;
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let context = BindingContext {
        run_input: &schedule.run_input,
        node_outputs: outputs,
        dependencies: &dependencies,
        item: None,
        item_key: None,
    };
    let resolved = resolve_bindings(items, &context)?;
    let mut round_items = resolved.as_array().cloned().ok_or_else(|| Error::Binding {
        path: format!("node.{}.operation.items", node.id),
        message: "reduce items must resolve to an array".to_string(),
    })?;
    let count = u64::try_from(round_items.len()).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} item count", node.id),
    })?;
    if count > max_items {
        return Err(Error::InvalidProjection {
            message: format!("reduce {} exceeds max_items", node.id),
        });
    }
    if round_items.len() <= 1 {
        return Ok(Vec::new());
    }

    let batch_size = usize::try_from(batch_size).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} batch_size", node.id),
    })?;
    let mut round = 1_u32;
    loop {
        let mut ready = Vec::new();
        let mut completed = Vec::new();
        for (batch_index, batch) in round_items.chunks(batch_size).enumerate() {
            let batch_index =
                u64::try_from(batch_index).map_err(|_| Error::ArithmeticOverflow {
                    context: format!("reduce {} batch index", node.id),
                })?;
            let item_key = format!("r{round}:b{batch_index}");
            if let Some(existing) = schedule
                .projection
                .tasks
                .iter()
                .find(|task| task.node_id == node.id && task.item_key == item_key)
            {
                if let Some(output) = completed_output(existing) {
                    validate_instance(
                        &node.output_schema,
                        &output,
                        &format!("node.{}.reduce.{item_key}", node.id),
                    )?;
                    completed.push(output);
                }
                continue;
            }
            let input = json!({
                "round": round,
                "batch_index": batch_index,
                "items": batch,
            });
            validate_reducer_capability_input(schedule, reducer, &input)?;
            let reservation = reducer_reservation(schedule, reducer, node.retry.max_attempts)?;
            ready.push(logical_task(
                schedule,
                node,
                item_key,
                input,
                reducer_kind(reducer),
                reservation,
            )?);
        }
        if !ready.is_empty() {
            return Ok(ready);
        }
        let expected = round_items.len().div_ceil(batch_size);
        if completed.len() != expected {
            return Ok(Vec::new());
        }
        if completed.len() == 1 {
            return Ok(Vec::new());
        }
        round_items = completed;
        round = round
            .checked_add(1)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: format!("reduce {} round", node.id),
            })?;
    }
}

fn logical_task(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    item_key: String,
    input: Value,
    kind: LogicalTaskKind,
    reservation: ExecutionEstimate,
) -> Result<LogicalTask> {
    Ok(LogicalTask {
        task_id: ExecutionTaskId::derive(request.run_uid, &node.id, &item_key)?,
        node_id: node.id.clone(),
        item_key,
        requirement_ids: node.requirement_ids.clone(),
        plan_revision: request.projection.plan_revision,
        generation: 1,
        input,
        kind,
        retry: node.retry.clone(),
        reservation,
    })
}

fn logical_kind(
    operation: &ExecutionOperation,
    context: &BindingContext<'_>,
) -> Result<LogicalTaskKind> {
    match operation {
        ExecutionOperation::Capability { reference } => Ok(LogicalTaskKind::Capability {
            reference: reference.clone(),
        }),
        ExecutionOperation::Agent {
            instructions,
            skill_refs,
            capability_refs,
            max_turns,
        } => Ok(LogicalTaskKind::Agent {
            instructions: instructions.clone(),
            skill_refs: skill_refs.clone(),
            capability_refs: capability_refs.clone(),
            max_turns: *max_turns,
        }),
        ExecutionOperation::Review { prompt } => Ok(LogicalTaskKind::Review {
            prompt: prompt.clone(),
        }),
        ExecutionOperation::WaitSignal { signal_name } => Ok(LogicalTaskKind::WaitSignal {
            signal_name: signal_name.clone(),
        }),
        ExecutionOperation::Output { value } => Ok(LogicalTaskKind::Output {
            value: resolve_bindings(value, context)?,
        }),
        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. } => {
            Err(Error::InvalidProjection {
                message: "aggregate operation cannot become an ordinary task".to_string(),
            })
        }
    }
}

fn map_kind(task: &MapTask) -> LogicalTaskKind {
    match task {
        MapTask::Capability { reference } => LogicalTaskKind::Capability {
            reference: reference.clone(),
        },
        MapTask::Agent {
            instructions,
            skill_refs,
            capability_refs,
            max_turns,
        } => LogicalTaskKind::Agent {
            instructions: instructions.clone(),
            skill_refs: skill_refs.clone(),
            capability_refs: capability_refs.clone(),
            max_turns: *max_turns,
        },
    }
}

fn reducer_kind(reducer: &ExecutionReducer) -> LogicalTaskKind {
    match reducer {
        ExecutionReducer::Capability { reference } => LogicalTaskKind::Capability {
            reference: reference.clone(),
        },
        ExecutionReducer::Agent {
            instructions,
            skill_refs,
            capability_refs,
            max_turns,
        } => LogicalTaskKind::Agent {
            instructions: instructions.clone(),
            skill_refs: skill_refs.clone(),
            capability_refs: capability_refs.clone(),
            max_turns: *max_turns,
        },
    }
}

fn operation_reservation(
    request: &ScheduleRequest,
    operation: &ExecutionOperation,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    match operation {
        ExecutionOperation::Agent { max_turns, .. } => {
            turn_reservation(&request.config, *max_turns, attempts, false)
        }
        ExecutionOperation::Capability { reference } => {
            capability_reservation(request, reference, attempts)
        }
        ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => Ok(ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        }),
        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. } => {
            Err(Error::InvalidProjection {
                message: "aggregate operation needs a task-specific reservation".to_string(),
            })
        }
    }
}

fn map_task_reservation(
    request: &ScheduleRequest,
    task: &MapTask,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    match task {
        MapTask::Capability { reference } => capability_reservation(request, reference, attempts),
        MapTask::Agent { max_turns, .. } => {
            turn_reservation(&request.config, *max_turns, attempts, false)
        }
    }
}

fn reducer_reservation(
    request: &ScheduleRequest,
    reducer: &ExecutionReducer,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    match reducer {
        ExecutionReducer::Capability { reference } => {
            capability_reservation(request, reference, attempts)
        }
        ExecutionReducer::Agent { max_turns, .. } => {
            turn_reservation(&request.config, *max_turns, attempts, false)
        }
    }
}

fn turn_reservation(
    config: &ExecutionConfig,
    max_turns: u32,
    attempts: u32,
    verifier: bool,
) -> Result<ExecutionEstimate> {
    let estimate = if verifier {
        ExecutionEstimate {
            cost_microusd: config.verifier_turn_cost_microusd,
            tokens: config.verifier_turn_tokens,
            tool_calls: config.verifier_turn_tool_calls,
            retrieved_bytes: config.verifier_turn_retrieved_bytes,
            tasks: 1,
        }
    } else {
        ExecutionEstimate {
            cost_microusd: config.agent_turn_cost_microusd,
            tokens: config.agent_turn_tokens,
            tool_calls: config.agent_turn_tool_calls,
            retrieved_bytes: config.agent_turn_retrieved_bytes,
            tasks: 1,
        }
    };
    estimate
        .checked_multiply_resources(u64::from(max_turns), "task turns")?
        .checked_multiply_resources(u64::from(attempts), "task retries")
}

fn capability_reservation(
    request: &ScheduleRequest,
    reference: &CapabilityReference,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    find_capability(&request.catalog, reference)?
        .estimate
        .checked_multiply_resources(u64::from(attempts), "capability retry reservation")
}

fn validate_capability_input(
    request: &ScheduleRequest,
    operation: &ExecutionOperation,
    input: &Value,
) -> Result<()> {
    if let ExecutionOperation::Capability { reference } = operation {
        validate_instance(
            &find_capability(&request.catalog, reference)?.input_schema,
            input,
            "logical_task.input",
        )?;
    }
    Ok(())
}

fn validate_map_capability_input(
    request: &ScheduleRequest,
    task: &MapTask,
    input: &Value,
) -> Result<()> {
    if let MapTask::Capability { reference } = task {
        validate_instance(
            &find_capability(&request.catalog, reference)?.input_schema,
            input,
            "logical_map_task.input",
        )?;
    }
    Ok(())
}

fn validate_reducer_capability_input(
    request: &ScheduleRequest,
    reducer: &ExecutionReducer,
    input: &Value,
) -> Result<()> {
    if let ExecutionReducer::Capability { reference } = reducer {
        validate_instance(
            &find_capability(&request.catalog, reference)?.input_schema,
            input,
            "logical_reducer_task.input",
        )?;
    }
    Ok(())
}

fn find_capability<'a>(
    catalog: &'a ExecutionCapabilityCatalog,
    reference: &CapabilityReference,
) -> Result<&'a ExecutionCapability> {
    catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == *reference)
        .ok_or_else(|| Error::InvalidProjection {
            message: format!(
                "capability {}@{} is absent from the pinned catalog",
                reference.name, reference.version
            ),
        })
}

fn waiting_reasons(
    request: &ScheduleRequest,
    dependency_waits: BTreeSet<String>,
) -> Vec<WaitingReason> {
    let by_id = request
        .plan
        .definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let mut waiting = Vec::new();
    if request.projection.tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::Pending
                | ExecutionTaskStatus::Reserved
                | ExecutionTaskStatus::Running
                | ExecutionTaskStatus::WaitingReplan
        )
    }) {
        waiting.push(WaitingReason::RunningTasks);
    }
    for task in &request.projection.tasks {
        if task.status == ExecutionTaskStatus::WaitingInput
            && let Some(outcome) = &task.outcome
            && let ExecutionTaskResult::NeedsInput { question, audience } = &outcome.result
        {
            waiting.push(WaitingReason::Input {
                task_id: task.task_id,
                audience: audience.clone(),
                question: question.clone(),
            });
        }
        if request.projection.node_statuses.get(&task.node_id)
            == Some(&ExecutionNodeStatus::Waiting)
            && let Some(node) = by_id.get(task.node_id.as_str())
        {
            match &node.operation {
                ExecutionOperation::Review { prompt } => waiting.push(WaitingReason::Review {
                    task_id: task.task_id,
                    prompt: prompt.clone(),
                }),
                ExecutionOperation::WaitSignal { signal_name } => {
                    waiting.push(WaitingReason::Signal {
                        task_id: task.task_id,
                        signal_name: signal_name.clone(),
                    });
                }
                ExecutionOperation::Capability { .. }
                | ExecutionOperation::Agent { .. }
                | ExecutionOperation::Map { .. }
                | ExecutionOperation::Reduce { .. }
                | ExecutionOperation::Output { .. } => {}
            }
        }
    }
    if !dependency_waits.is_empty() {
        waiting.push(WaitingReason::Dependencies {
            node_ids: dependency_waits.into_iter().collect(),
        });
    }
    waiting
}

fn schedule_verifiers_or_complete(request: ScheduleRequest) -> Result<ScheduleDecision> {
    let terminal = terminal_output(&request.plan, &request.projection);
    let preliminary = evaluate_completion(CompletionEvaluationRequest {
        goal: request.goal.clone(),
        plan: request.plan.clone(),
        run_input: request.run_input.clone(),
        projection: request.projection.clone(),
        terminal_output: terminal.clone(),
        budget_ledger: request.budget_ledger.clone(),
        now: request.now,
    })?;
    let unresolved = preliminary.unsatisfied_requirement_ids;
    let summaries = verifier_summaries(&request.plan, &request.projection)?;
    let mut ready = Vec::new();
    for check in &request.goal.completion_checks {
        let CompletionCheckKind::AgentVerifier {
            instructions,
            max_turns,
        } = &check.kind
        else {
            continue;
        };
        let node_id = format!("@check/{}", check.id);
        let item_key = format!("check:{}", check.id);
        if request
            .projection
            .tasks
            .iter()
            .any(|task| task.node_id == node_id)
        {
            continue;
        }
        let reservation = turn_reservation(&request.config, *max_turns, 1, true)?;
        ready.push(LogicalTask {
            task_id: ExecutionTaskId::derive(request.run_uid, &node_id, &item_key)?,
            node_id,
            item_key,
            requirement_ids: unresolved.clone(),
            plan_revision: request.projection.plan_revision,
            generation: 1,
            input: json!({
                "goal": &request.goal,
                "check_id": &check.id,
                "description": &check.description,
                "terminal_output": &terminal,
                "task_summaries": &summaries,
            }),
            kind: LogicalTaskKind::CompletionVerifier {
                check_id: check.id.clone(),
                instructions: instructions.clone(),
                max_turns: *max_turns,
            },
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            reservation,
        });
    }
    if !ready.is_empty() {
        let mut ledger = request.budget_ledger.clone();
        for task in &ready {
            if ledger.try_reserve(task.reservation).is_err() {
                return Ok(budget_terminal(terminal));
            }
        }
        return Ok(ScheduleDecision::Ready(ready));
    }
    if request.projection.tasks.iter().any(|task| {
        task.node_id.starts_with("@check/")
            && !matches!(
                task.status,
                ExecutionTaskStatus::Completed
                    | ExecutionTaskStatus::Failed
                    | ExecutionTaskStatus::Cancelled
            )
    }) {
        let waiting = waiting_reasons(&request, BTreeSet::new());
        return Ok(ScheduleDecision::Waiting(if waiting.is_empty() {
            vec![WaitingReason::RunningTasks]
        } else {
            waiting
        }));
    }
    completion_terminal(&request, terminal)
}

fn verifier_summaries(
    plan: &CanonicalExecutionPlan,
    projection: &ExecutionProjection,
) -> Result<Vec<VerifierTaskSummary>> {
    let mut summaries = projection
        .tasks
        .iter()
        .filter(|task| !task.node_id.starts_with("@check/"))
        .filter(|task| is_terminal_task_status(task.status))
        .map(|task| {
            let output_hash = completed_output(task)
                .as_ref()
                .map(task_output_hash)
                .transpose()?;
            let failure = task
                .outcome
                .as_ref()
                .and_then(|outcome| match &outcome.result {
                    ExecutionTaskResult::Failed { class, message } => Some(ExecutionTaskFailure {
                        class: class.clone(),
                        message: message.clone(),
                        capability_ref: plan
                            .definition
                            .nodes
                            .iter()
                            .find(|node| node.id == task.node_id)
                            .and_then(|node| operation_capability(&node.operation)),
                    }),
                    ExecutionTaskResult::Cancelled { reason } => Some(ExecutionTaskFailure {
                        class: ExecutionFailureClass::Cancelled,
                        message: reason.clone(),
                        capability_ref: None,
                    }),
                    ExecutionTaskResult::Completed { .. }
                    | ExecutionTaskResult::NeedsInput { .. }
                    | ExecutionTaskResult::NeedsReplan { .. } => None,
                });
            let mut citation_source_ids = task
                .outcome
                .as_ref()
                .and_then(|outcome| match &outcome.result {
                    ExecutionTaskResult::Completed { citations, .. } => Some(
                        citations
                            .iter()
                            .filter(|citation| !citation.source_id.trim().is_empty())
                            .map(|citation| citation.source_id.clone())
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            citation_source_ids.sort();
            citation_source_ids.dedup();
            Ok(VerifierTaskSummary {
                task_id: task.task_id,
                node_id: task.node_id.clone(),
                item_key: task.item_key.clone(),
                status: task.status,
                output_hash,
                failure,
                citation_source_ids,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    summaries.sort_by(|left, right| {
        (&left.node_id, &left.item_key, left.task_id).cmp(&(
            &right.node_id,
            &right.item_key,
            right.task_id,
        ))
    });
    Ok(summaries)
}

fn budget_terminal(output: Option<Value>) -> ScheduleDecision {
    match output {
        Some(output) => ScheduleDecision::Terminal(TerminalProjection::Partial {
            output: Some(output),
            gaps: vec!["execution budget cannot reserve required work".to_string()],
        }),
        None => ScheduleDecision::Terminal(TerminalProjection::Failed {
            failure: ExecutionTaskFailure {
                class: ExecutionFailureClass::BudgetExceeded,
                message: "required task reservation exceeds the remaining run budget".to_string(),
                capability_ref: None,
            },
        }),
    }
}

fn completion_terminal(
    request: &ScheduleRequest,
    terminal_output: Option<Value>,
) -> Result<ScheduleDecision> {
    if request.plan.definition.nodes.iter().all(|node| {
        request.projection.node_statuses.get(&node.id) == Some(&ExecutionNodeStatus::Cancelled)
    }) {
        return Ok(ScheduleDecision::Terminal(TerminalProjection::Cancelled {
            reason: "all execution nodes were cancelled".to_string(),
        }));
    }
    let evaluation = evaluate_completion(CompletionEvaluationRequest {
        goal: request.goal.clone(),
        plan: request.plan.clone(),
        run_input: request.run_input.clone(),
        projection: request.projection.clone(),
        terminal_output: terminal_output.clone(),
        budget_ledger: request.budget_ledger.clone(),
        now: request.now,
    })?;
    let terminal = match evaluation.status {
        CompletionStatus::Completed => TerminalProjection::Completed {
            output: terminal_output.ok_or_else(|| Error::InvalidProjection {
                message: "completed evaluation has no terminal output".to_string(),
            })?,
        },
        CompletionStatus::Partial => TerminalProjection::Partial {
            output: terminal_output,
            gaps: evaluation.gaps,
        },
        CompletionStatus::Blocked => TerminalProjection::Blocked {
            output: terminal_output,
            gaps: evaluation.gaps,
        },
        CompletionStatus::Unsupported => TerminalProjection::Unsupported {
            reason: "required execution paths are unsupported".to_string(),
            gaps: evaluation.gaps,
        },
        CompletionStatus::Failed => TerminalProjection::Failed {
            failure: terminal_failure(request, &evaluation.gaps),
        },
    };
    Ok(ScheduleDecision::Terminal(terminal))
}

fn terminal_failure(request: &ScheduleRequest, gaps: &[String]) -> ExecutionTaskFailure {
    request
        .projection
        .tasks
        .iter()
        .find_map(|task| {
            task.outcome
                .as_ref()
                .and_then(|outcome| match &outcome.result {
                    ExecutionTaskResult::Failed { class, message } => Some(ExecutionTaskFailure {
                        class: class.clone(),
                        message: message.clone(),
                        capability_ref: request
                            .plan
                            .definition
                            .nodes
                            .iter()
                            .find(|node| node.id == task.node_id)
                            .and_then(|node| operation_capability(&node.operation)),
                    }),
                    _ => None,
                })
        })
        .unwrap_or_else(|| ExecutionTaskFailure {
            class: ExecutionFailureClass::Terminal,
            message: if gaps.is_empty() {
                "execution did not produce a complete result".to_string()
            } else {
                gaps.join("; ")
            },
            capability_ref: None,
        })
}

fn terminal_output(
    plan: &CanonicalExecutionPlan,
    projection: &ExecutionProjection,
) -> Option<Value> {
    let output_node_id = plan
        .definition
        .nodes
        .iter()
        .find(|node| matches!(node.operation, ExecutionOperation::Output { .. }))
        .map(|node| node.id.as_str())?;
    projection
        .tasks
        .iter()
        .filter(|task| task.node_id == output_node_id)
        .filter(|task| task.item_key.is_empty())
        .filter(|task| task.status == ExecutionTaskStatus::Completed)
        .find_map(completed_output)
}

fn operation_capability(operation: &ExecutionOperation) -> Option<CapabilityReference> {
    match operation {
        ExecutionOperation::Capability { reference } => Some(reference.clone()),
        ExecutionOperation::Map {
            task: MapTask::Capability { reference },
            ..
        }
        | ExecutionOperation::Reduce {
            reducer: ExecutionReducer::Capability { reference },
            ..
        } => Some(reference.clone()),
        ExecutionOperation::Agent { .. }
        | ExecutionOperation::Map { .. }
        | ExecutionOperation::Reduce { .. }
        | ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => None,
    }
}

fn effective_status(
    statuses: &BTreeMap<String, ExecutionNodeStatus>,
    node_id: &str,
) -> Option<ExecutionNodeStatus> {
    Some(
        statuses
            .get(node_id)
            .copied()
            .unwrap_or(ExecutionNodeStatus::Pending),
    )
}

const fn is_terminal_node_status(status: ExecutionNodeStatus) -> bool {
    matches!(
        status,
        ExecutionNodeStatus::Completed
            | ExecutionNodeStatus::Skipped
            | ExecutionNodeStatus::Failed
            | ExecutionNodeStatus::Cancelled
    )
}

const fn is_terminal_task_status(status: ExecutionTaskStatus) -> bool {
    matches!(
        status,
        ExecutionTaskStatus::Completed
            | ExecutionTaskStatus::Skipped
            | ExecutionTaskStatus::Failed
            | ExecutionTaskStatus::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use moa_artifacts::execution_plan::{
        ExecutionBudgetLimit, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
        ExecutionPlanDefinition, MapTask, RetryPolicy,
    };

    use super::*;
    use crate::{
        capability::{ExecutionCapabilityCatalog, ExecutionEstimate, ExecutionHash},
        compiler::ExecutionValidationReport,
    };

    #[test]
    fn empty_map_is_reported_as_first_materialization_without_a_logical_task() {
        // Pins: a valid zero-item map produces a durable marker candidate even though
        // `schedule` cannot return a task row for it.
        let catalog = ExecutionCapabilityCatalog::build(Vec::new()).expect("build empty catalog");
        let map_node = ExecutionNode {
            id: "empty-map".to_string(),
            requirement_ids: Vec::new(),
            depends_on: Vec::new(),
            when: None,
            input: serde_json::json!({}),
            output_schema: serde_json::json!({}),
            operation: ExecutionOperation::Map {
                items: serde_json::json!([]),
                item_key: String::new(),
                max_items: 4,
                item_output_schema: serde_json::json!({}),
                task: MapTask::Agent {
                    instructions: "inspect".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: Vec::new(),
                    max_turns: 1,
                },
            },
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
            },
            budget: None,
        };
        let request = ScheduleRequest {
            run_uid: Uuid::now_v7(),
            goal: ExecutionGoalContract {
                objective: "accept empty input".to_string(),
                requirements: Vec::new(),
                deliverables: Vec::new(),
                coverage: Vec::new(),
                constraints: Vec::new(),
                completion_checks: Vec::new(),
            },
            plan: CanonicalExecutionPlan {
                definition: ExecutionPlanDefinition {
                    schema_version: 1,
                    input_schema: serde_json::json!({}),
                    output_schema: serde_json::json!({}),
                    nodes: vec![map_node],
                },
                plan_hash: ExecutionHash::from_bytes([1; 32]),
                catalog_hash: catalog.catalog_hash,
                estimate: ExecutionEstimate::default(),
                report: ExecutionValidationReport::default(),
            },
            catalog,
            run_input: serde_json::json!({}),
            projection: ExecutionProjection {
                plan_revision: 1,
                node_statuses: BTreeMap::new(),
                tasks: Vec::new(),
            },
            config: ExecutionConfig::default(),
            budget_ledger: BudgetLedger::new(ExecutionBudgetLimit {
                max_cost_microusd: None,
                max_tokens: None,
                max_tasks: Some(10),
                max_tool_calls: None,
                max_retrieved_bytes: None,
                deadline_at: Some(Utc::now() + Duration::hours(1)),
            }),
            now: Utc::now(),
        };

        assert_eq!(
            ready_empty_map_nodes(&request).expect("derive empty map marker"),
            vec!["empty-map".to_string()]
        );

        let mut nonempty = request;
        let ExecutionOperation::Map { items, .. } =
            &mut nonempty.plan.definition.nodes[0].operation
        else {
            unreachable!("test plan node must remain a map");
        };
        *items = serde_json::json!([{"id": 1}]);
        assert!(
            ready_empty_map_nodes(&nonempty)
                .expect("derive nonempty map")
                .is_empty()
        );
    }
}
