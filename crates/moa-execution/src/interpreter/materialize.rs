//! Logical-task materialization for executable plan nodes.

use super::*;

pub(super) fn materialize_node(
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

pub(super) struct MapMaterializationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    item_key_pointer: &'a str,
    max_items: u64,
    task: &'a MapTask,
}

pub(super) fn materialize_map(request: MapMaterializationRequest<'_>) -> Result<Vec<LogicalTask>> {
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

pub(super) struct ReduceMaterializationRequest<'a> {
    schedule: &'a ScheduleRequest,
    node: &'a ExecutionNode,
    outputs: &'a BTreeMap<String, Value>,
    items: &'a Value,
    max_items: u64,
    reducer: &'a ExecutionReducer,
    batch_size: u32,
}

pub(super) fn materialize_reduce(
    request: ReduceMaterializationRequest<'_>,
) -> Result<Vec<LogicalTask>> {
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

pub(super) fn logical_task(
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

pub(super) fn logical_kind(
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

pub(super) fn map_kind(task: &MapTask) -> LogicalTaskKind {
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

pub(super) fn reducer_kind(reducer: &ExecutionReducer) -> LogicalTaskKind {
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
