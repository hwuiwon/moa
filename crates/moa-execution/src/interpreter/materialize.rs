//! Logical-task materialization for executable plan nodes.

use super::*;

pub(super) fn materialize_node_page(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    outputs: &BTreeMap<String, Value>,
    cursor: u64,
    limit: u32,
    reduce: Option<&ReduceMaterializationPageInput>,
) -> Result<NodeMaterializationPage> {
    if let ExecutionOperation::Map {
        items,
        item_key,
        max_items,
        task,
        ..
    } = &node.operation
    {
        return materialize_map_page(
            request, node, outputs, items, item_key, *max_items, task, cursor, limit,
        );
    }
    if let ExecutionOperation::Reduce {
        items,
        max_items,
        reducer,
        batch_size,
    } = &node.operation
    {
        let reduce = reduce.ok_or_else(|| Error::InvalidProjection {
            message: format!("reduce node {} requires a persisted round cursor", node.id),
        })?;
        return materialize_reduce_page(
            request,
            node,
            outputs,
            items,
            *max_items,
            reducer,
            *batch_size,
            limit,
            reduce,
        );
    }
    if reduce.is_some() {
        return Err(Error::InvalidProjection {
            message: format!("non-reduce node {} received a reduce cursor", node.id),
        });
    }
    if cursor > 0 {
        return Ok(NodeMaterializationPage {
            tasks: Vec::new(),
            next_cursor: cursor,
            source_exhausted: true,
            reduce_cursor: None,
            terminal_output: None,
            condition_skipped: false,
        });
    }
    let mut tasks = materialize_single_task(request, node, outputs)?;
    let limit = usize::try_from(limit).map_err(|_| Error::ArithmeticOverflow {
        context: format!("node {} materialization page limit", node.id),
    })?;
    let source_exhausted = tasks.len() <= limit;
    tasks.truncate(limit);
    let next_cursor = u64::try_from(tasks.len()).map_err(|_| Error::ArithmeticOverflow {
        context: format!("node {} materialization page cursor", node.id),
    })?;
    Ok(NodeMaterializationPage {
        tasks,
        next_cursor,
        source_exhausted,
        reduce_cursor: None,
        terminal_output: None,
        condition_skipped: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn materialize_reduce_page(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    outputs: &BTreeMap<String, Value>,
    items: &Value,
    max_items: u64,
    reducer: &ExecutionReducer,
    batch_size: u32,
    limit: u32,
    cursor: &ReduceMaterializationPageInput,
) -> Result<NodeMaterializationPage> {
    if cursor.round == 0 {
        return Err(Error::InvalidProjection {
            message: format!("reduce node {} round must be one-based", node.id),
        });
    }
    let batch_size = usize::try_from(batch_size).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} batch size", node.id),
    })?;
    let limit = usize::try_from(limit).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} page limit", node.id),
    })?;
    let owned_round_one;
    let (page_inputs, round_input_count) = if cursor.round == 1 {
        if !cursor.page_inputs.is_empty() {
            return Err(Error::InvalidProjection {
                message: format!(
                    "reduce node {} round one reads immutable plan items",
                    node.id
                ),
            });
        }
        let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
        let resolved = resolve_bindings(
            items,
            &BindingContext {
                run_input: &request.run_input,
                node_outputs: outputs,
                dependencies: &dependencies,
                item: None,
                item_key: None,
            },
        )?;
        owned_round_one = resolved.as_array().cloned().ok_or_else(|| Error::Binding {
            path: format!("node.{}.operation.items", node.id),
            message: "reduce items must resolve to an array".to_string(),
        })?;
        let actual_count =
            u64::try_from(owned_round_one.len()).map_err(|_| Error::ArithmeticOverflow {
                context: format!("reduce {} round-one input count", node.id),
            })?;
        if actual_count == 0 {
            return Err(Error::InvalidProjection {
                message: format!("reduce {} requires at least one item", node.id),
            });
        }
        if cursor
            .round_input_count
            .is_some_and(|persisted| persisted != actual_count)
            || actual_count > max_items
        {
            return Err(Error::InvalidProjection {
                message: format!("reduce node {} round-one input count changed", node.id),
            });
        }
        if actual_count == 1 {
            let output = owned_round_one[0].clone();
            validate_instance(
                &node.output_schema,
                &output,
                &format!("node.{}.output", node.id),
            )?;
            return Ok(NodeMaterializationPage {
                tasks: Vec::new(),
                next_cursor: cursor.batch_cursor,
                source_exhausted: true,
                reduce_cursor: Some(ReduceMaterializationCursor {
                    round: cursor.round,
                    batch_cursor: cursor.batch_cursor,
                    round_input_count: actual_count,
                }),
                terminal_output: Some(output),
                condition_skipped: false,
            });
        }
        let start = usize::try_from(cursor.batch_cursor)
            .ok()
            .and_then(|batch| batch.checked_mul(batch_size))
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: format!("reduce {} round-one page start", node.id),
            })?;
        let requested_items =
            limit
                .checked_mul(batch_size)
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: format!("reduce {} round-one page length", node.id),
                })?;
        let end = start
            .saturating_add(requested_items)
            .min(owned_round_one.len());
        (
            owned_round_one
                .get(start..end)
                .ok_or_else(|| Error::InvalidProjection {
                    message: format!("reduce node {} batch cursor exceeds round input", node.id),
                })?,
            actual_count,
        )
    } else {
        let round_input_count =
            cursor
                .round_input_count
                .ok_or_else(|| Error::InvalidProjection {
                    message: format!(
                        "reduce node {} later round requires its persisted input count",
                        node.id
                    ),
                })?;
        let start = cursor
            .batch_cursor
            .checked_mul(
                u64::try_from(batch_size).map_err(|_| Error::ArithmeticOverflow {
                    context: format!("reduce {} batch size", node.id),
                })?,
            )
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: format!("reduce {} page input offset", node.id),
            })?;
        let remaining = round_input_count.saturating_sub(start);
        let expected = remaining.min(
            u64::try_from(limit)
                .map_err(|_| Error::ArithmeticOverflow {
                    context: format!("reduce {} page limit", node.id),
                })?
                .checked_mul(
                    u64::try_from(batch_size).map_err(|_| Error::ArithmeticOverflow {
                        context: format!("reduce {} batch size", node.id),
                    })?,
                )
                .ok_or_else(|| Error::ArithmeticOverflow {
                    context: format!("reduce {} page input length", node.id),
                })?,
        );
        if u64::try_from(cursor.page_inputs.len()).map_err(|_| Error::ArithmeticOverflow {
            context: format!("reduce {} page input count", node.id),
        })? != expected
        {
            return Err(Error::InvalidProjection {
                message: format!("reduce node {} page input slice is not contiguous", node.id),
            });
        }
        (cursor.page_inputs.as_slice(), round_input_count)
    };

    let mut tasks = Vec::with_capacity(page_inputs.len().div_ceil(batch_size));
    for (page_batch, batch) in page_inputs.chunks(batch_size).enumerate() {
        let batch_index = cursor
            .batch_cursor
            .checked_add(
                u64::try_from(page_batch).map_err(|_| Error::ArithmeticOverflow {
                    context: format!("reduce {} page batch index", node.id),
                })?,
            )
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: format!("reduce {} batch index", node.id),
            })?;
        let item_key = format!("r{}:b{batch_index}", cursor.round);
        let input = json!({
            "round": cursor.round,
            "batch_index": batch_index,
            "items": batch,
        });
        validate_reducer_capability_input(request, reducer, &input)?;
        let reservation = reducer_reservation(request, reducer, node.retry.max_attempts)?;
        tasks.push(logical_task(
            request,
            node,
            item_key,
            input,
            reducer_kind(reducer),
            reservation,
        )?);
    }
    let materialized = u64::try_from(tasks.len()).map_err(|_| Error::ArithmeticOverflow {
        context: format!("reduce {} materialized batch count", node.id),
    })?;
    let next_cursor = cursor
        .batch_cursor
        .checked_add(materialized)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: format!("reduce {} next batch cursor", node.id),
        })?;
    let total_batches = round_input_count.div_ceil(u64::try_from(batch_size).map_err(|_| {
        Error::ArithmeticOverflow {
            context: format!("reduce {} batch size", node.id),
        }
    })?);
    Ok(NodeMaterializationPage {
        tasks,
        next_cursor,
        source_exhausted: next_cursor == total_batches,
        reduce_cursor: Some(ReduceMaterializationCursor {
            round: cursor.round,
            batch_cursor: cursor.batch_cursor,
            round_input_count,
        }),
        terminal_output: None,
        condition_skipped: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn materialize_map_page(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    outputs: &BTreeMap<String, Value>,
    items: &Value,
    item_key_pointer: &str,
    max_items: u64,
    task: &MapTask,
    cursor: u64,
    limit: u32,
) -> Result<NodeMaterializationPage> {
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let base = BindingContext {
        run_input: &request.run_input,
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
    if cursor > count {
        return Err(Error::InvalidProjection {
            message: format!("map {} materialization cursor exceeds item count", node.id),
        });
    }
    if values.is_empty() {
        let output = json!({ "items": [] });
        validate_instance(
            &node.output_schema,
            &output,
            &format!("node.{}.output", node.id),
        )?;
        return Ok(NodeMaterializationPage {
            tasks: Vec::new(),
            next_cursor: cursor,
            source_exhausted: true,
            reduce_cursor: None,
            terminal_output: Some(output),
            condition_skipped: false,
        });
    }
    let start = usize::try_from(cursor).map_err(|_| Error::ArithmeticOverflow {
        context: format!("map {} materialization cursor", node.id),
    })?;
    // Duplicate item keys are checked across the WHOLE resolved array, not just this
    // page. A per-page check misses a duplicate that spans a page boundary, and that
    // case does not fail loudly: both items derive the same `ExecutionTaskId`, the
    // insert hits `ON CONFLICT ... DO NOTHING`, materialization returns `Conflict`, and
    // the controller reschedules the same page forever until the run deadline. Paying
    // one O(n) scan on the first page turns that livelock back into a deterministic
    // plan error, which is what the pre-paging implementation did.
    if start == 0 {
        let mut all_keys = BTreeSet::new();
        for item in values.iter() {
            let item_key = extract_map_key(item, item_key_pointer)?;
            if !all_keys.insert(item_key.clone()) {
                return Err(Error::InvalidProjection {
                    message: format!("map {} produced duplicate item key {item_key}", node.id),
                });
            }
        }
    }
    let end_u64 = cursor.saturating_add(u64::from(limit)).min(count);
    let end = usize::try_from(end_u64).map_err(|_| Error::ArithmeticOverflow {
        context: format!("map {} materialization page end", node.id),
    })?;
    let mut page = Vec::with_capacity(end.saturating_sub(start));
    let mut page_keys = BTreeSet::new();
    for item in &values[start..end] {
        let item_key = extract_map_key(item, item_key_pointer)?;
        if !page_keys.insert(item_key.clone()) {
            return Err(Error::InvalidProjection {
                message: format!("map {} produced duplicate item key {item_key}", node.id),
            });
        }
        let context = BindingContext {
            item: Some(item),
            item_key: Some(&item_key),
            ..base
        };
        let input = resolve_bindings(&node.input, &context)?;
        validate_map_capability_input(request, task, &input)?;
        let reservation = map_task_reservation(request, task, node.retry.max_attempts)?;
        page.push(logical_task(
            request,
            node,
            item_key,
            input,
            map_kind(task),
            reservation,
        )?);
    }
    Ok(NodeMaterializationPage {
        tasks: page,
        next_cursor: end_u64,
        source_exhausted: end_u64 == count,
        reduce_cursor: None,
        terminal_output: None,
        condition_skipped: false,
    })
}

/// Materializes the single logical task of one non-aggregate node.
///
/// Map and reduce nodes are paged by `materialize_map_page` and
/// `materialize_reduce_page` before this is reached.
fn materialize_single_task(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    outputs: &BTreeMap<String, Value>,
) -> Result<Vec<LogicalTask>> {
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
    let reservation = operation_reservation(request, &node.operation, node.retry.max_attempts)?;
    Ok(vec![logical_task(
        request,
        node,
        String::new(),
        input,
        kind,
        reservation,
    )?])
}

pub(super) fn logical_task(
    request: &ScheduleRequest,
    node: &ExecutionNode,
    item_key: String,
    input: Value,
    kind: LogicalTaskKind,
    reservation: ExecutionEstimate,
) -> Result<LogicalTask> {
    let compensation =
        if item_key.is_empty() && matches!(node.operation, ExecutionOperation::Capability { .. }) {
            node.compensation.clone()
        } else {
            None
        };
    Ok(LogicalTask {
        task_id: ExecutionTaskId::derive(request.run_uid, &node.id, &item_key)?,
        node_id: node.id.clone(),
        item_key,
        requirement_ids: node.requirement_ids.clone(),
        plan_revision: request.projection.plan_revision,
        generation: 1,
        input,
        kind,
        compensation,
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
        ExecutionOperation::Review {
            prompt,
            wait_policy,
        } => Ok(LogicalTaskKind::Review {
            prompt: prompt.clone(),
            wait_policy: wait_policy.clone(),
        }),
        ExecutionOperation::WaitSignal {
            signal_name,
            wait_policy,
        } => Ok(LogicalTaskKind::WaitSignal {
            signal_name: signal_name.clone(),
            wait_policy: wait_policy.clone(),
        }),
        ExecutionOperation::WaitUntil { wake, result } => Ok(LogicalTaskKind::WaitUntil {
            wake: wake.clone(),
            result: resolve_bindings(result, context)?,
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
