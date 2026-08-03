//! Condition evaluation and deterministic aggregate-node derivation.

use super::*;

pub(super) fn apply_false_conditions(
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

pub(super) fn derive_aggregate_nodes(
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

pub(super) enum AggregateState {
    Pending,
    Completed(Value),
    Failed,
    Cancelled,
}

pub(super) struct MapDerivationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    item_key_pointer: &'a str,
    max_items: u64,
}

pub(super) fn derive_map_output(request: MapDerivationRequest<'_>) -> Result<AggregateState> {
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

pub(super) struct ReduceDerivationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    max_items: u64,
    batch_size: u32,
}

pub(super) fn derive_reduce_output(request: ReduceDerivationRequest<'_>) -> Result<AggregateState> {
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

pub(super) fn derive_reduce_rounds(
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
