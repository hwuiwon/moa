//! Deterministic plan and node resource estimation.

use super::*;

pub(super) fn estimate_plan(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) -> Option<ExecutionEstimate> {
    let catalog = capability_lookup(catalog);
    let mut total = ExecutionEstimate::default();
    for (index, node) in plan.nodes.iter().enumerate() {
        let Some(estimate) = estimate_node(node, &catalog, config, report, index) else {
            continue;
        };
        if let Some(limit) = &node.budget
            && let Err(error) = estimate_fits_limit(estimate, limit)
        {
            append_error(
                report,
                "node_budget_exceeded",
                format!("plan.nodes[{index}].budget"),
                error,
            );
        }
        match total.checked_add(estimate, "plan total estimate") {
            Ok(next) => total = next,
            Err(error) => append_error(
                report,
                "estimate_overflow",
                format!("plan.nodes[{index}]"),
                error,
            ),
        }
    }

    for (index, check) in goal.completion_checks.iter().enumerate() {
        let CompletionCheckKind::AgentVerifier { max_turns, .. } = check.kind else {
            continue;
        };
        let estimate = verifier_estimate(config, max_turns);
        match estimate.and_then(|estimate| total.checked_add(estimate, "verifier estimate")) {
            Ok(next) => total = next,
            Err(error) => append_error(
                report,
                "estimate_overflow",
                format!("goal.completion_checks[{index}]"),
                error,
            ),
        }
    }

    (!report.has_errors()).then_some(total)
}

pub(super) fn estimate_remaining_plan(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    projection: &ExecutionProjection,
    catalog: &ExecutionCapabilityCatalog,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) -> Option<ExecutionEstimate> {
    let lookup = capability_lookup(catalog);
    let mut total = ExecutionEstimate::default();
    for (index, node) in plan.nodes.iter().enumerate() {
        if projection
            .node_statuses
            .get(&node.id)
            .is_some_and(|status| {
                matches!(
                    status,
                    ExecutionNodeStatus::Completed
                        | ExecutionNodeStatus::Skipped
                        | ExecutionNodeStatus::Failed
                        | ExecutionNodeStatus::Cancelled
                )
            })
        {
            continue;
        }
        if let Some(estimate) = estimate_node(node, &lookup, config, report, index) {
            match total.checked_add(estimate, "remaining plan estimate") {
                Ok(next) => total = next,
                Err(error) => append_error(
                    report,
                    "estimate_overflow",
                    format!("plan.nodes[{index}]"),
                    error,
                ),
            }
        }
    }

    for (index, check) in goal.completion_checks.iter().enumerate() {
        let CompletionCheckKind::AgentVerifier { max_turns, .. } = check.kind else {
            continue;
        };
        let node_id = format!("@check/{}", check.id);
        if projection.tasks.iter().any(|task| {
            task.node_id == node_id
                && matches!(
                    task.status,
                    ExecutionTaskStatus::Completed
                        | ExecutionTaskStatus::Failed
                        | ExecutionTaskStatus::Cancelled
                )
        }) {
            continue;
        }
        match verifier_estimate(config, max_turns)
            .and_then(|estimate| total.checked_add(estimate, "remaining verifier estimate"))
        {
            Ok(next) => total = next,
            Err(error) => append_error(
                report,
                "estimate_overflow",
                format!("goal.completion_checks[{index}]"),
                error,
            ),
        }
    }
    (!report.has_errors()).then_some(total)
}

pub(super) fn capability_lookup(
    catalog: &ExecutionCapabilityCatalog,
) -> BTreeMap<Vec<u8>, &ExecutionCapability> {
    catalog
        .capabilities
        .iter()
        .filter_map(|capability| {
            canonical_sort_key(&capability.reference)
                .ok()
                .map(|key| (key, capability))
        })
        .collect()
}

pub(super) fn estimate_node(
    node: &ExecutionNode,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
    index: usize,
) -> Option<ExecutionEstimate> {
    let attempts = u64::from(node.retry.max_attempts);
    let path = format!("plan.nodes[{index}].operation");
    let result = match &node.operation {
        ExecutionOperation::Capability { reference } => capability_estimate(reference, catalog)
            .and_then(|estimate| {
                estimate.checked_multiply_resources(attempts, "capability retry estimate")
            })
            .and_then(|forward| {
                let Some(compensation) = &node.compensation else {
                    return Ok(forward);
                };
                let compensation = capability_estimate(&compensation.compensator, catalog)?
                    .checked_multiply_resources(
                        attempts,
                        "capability compensation retry estimate",
                    )?;
                forward.checked_add(compensation, "capability compensation estimate")
            }),
        ExecutionOperation::Agent { max_turns, .. } => {
            agent_estimate(config, *max_turns).and_then(|estimate| {
                estimate.checked_multiply_resources(attempts, "agent retry estimate")
            })
        }
        ExecutionOperation::Map {
            max_items, task, ..
        } => map_task_estimate(task, catalog, config).and_then(|estimate| {
            estimate
                .checked_multiply_resources(attempts, "map retry estimate")?
                .checked_multiply_all(*max_items, "map cardinality estimate")
        }),
        ExecutionOperation::Reduce {
            max_items,
            reducer,
            batch_size,
            ..
        } => reducer_estimate(reducer, catalog, config).and_then(|estimate| {
            let tasks = reducer_task_count(*max_items, u64::from(*batch_size))?;
            estimate
                .checked_multiply_resources(attempts, "reduce retry estimate")?
                .checked_multiply_all(tasks, "reduce hierarchy estimate")
        }),
        ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => Ok(ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        }),
    };

    match result {
        Ok(estimate) => Some(estimate),
        Err(error) => {
            append_error(report, "estimate_failed", path, error);
            None
        }
    }
}

pub(super) fn capability_estimate(
    reference: &moa_artifacts::execution_plan::CapabilityReference,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
) -> Result<ExecutionEstimate, Error> {
    let key = canonical_sort_key(reference)?;
    catalog
        .get(&key)
        .map(|capability| capability.estimate)
        .ok_or_else(|| Error::InvalidProjection {
            message: format!(
                "capability {}@{} is absent from the catalog",
                reference.name, reference.version
            ),
        })
}

pub(super) fn agent_estimate(
    config: &ExecutionConfig,
    max_turns: u32,
) -> Result<ExecutionEstimate, Error> {
    ExecutionEstimate {
        cost_microusd: config.agent_turn_cost_microusd,
        tokens: config.agent_turn_tokens,
        tool_calls: config.agent_turn_tool_calls,
        retrieved_bytes: config.agent_turn_retrieved_bytes,
        tasks: 1,
    }
    .checked_multiply_resources(u64::from(max_turns), "agent turn estimate")
}

pub(super) fn verifier_estimate(
    config: &ExecutionConfig,
    max_turns: u32,
) -> Result<ExecutionEstimate, Error> {
    ExecutionEstimate {
        cost_microusd: config.verifier_turn_cost_microusd,
        tokens: config.verifier_turn_tokens,
        tool_calls: config.verifier_turn_tool_calls,
        retrieved_bytes: config.verifier_turn_retrieved_bytes,
        tasks: 1,
    }
    .checked_multiply_resources(u64::from(max_turns), "verifier turn estimate")
}

pub(super) fn map_task_estimate(
    task: &MapTask,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    config: &ExecutionConfig,
) -> Result<ExecutionEstimate, Error> {
    match task {
        MapTask::Capability { reference } => capability_estimate(reference, catalog),
        MapTask::Agent { max_turns, .. } => agent_estimate(config, *max_turns),
    }
}

pub(super) fn reducer_estimate(
    reducer: &ExecutionReducer,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    config: &ExecutionConfig,
) -> Result<ExecutionEstimate, Error> {
    match reducer {
        ExecutionReducer::Capability { reference } => capability_estimate(reference, catalog),
        ExecutionReducer::Agent { max_turns, .. } => agent_estimate(config, *max_turns),
    }
}

pub(super) fn reducer_task_count(mut items: u64, batch_size: u64) -> Result<u64, Error> {
    if batch_size < 2 {
        return Err(Error::InvalidProjection {
            message: "reduce batch_size must be at least two".to_string(),
        });
    }
    let mut total = 0_u64;
    while items > 1 {
        let batches = items / batch_size + u64::from(!items.is_multiple_of(batch_size));
        total = total
            .checked_add(batches)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "reduce task count".to_string(),
            })?;
        items = batches;
    }
    Ok(total)
}
