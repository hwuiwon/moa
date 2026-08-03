//! Durable budget and scheduler projection reconstruction.

use super::*;

pub(super) fn budget_ledger(run: &ExecutionRunRecord) -> BudgetLedger {
    BudgetLedger {
        limit: run.approved_budget.clone(),
        reserved: run.reserved,
        consumed: run.consumed,
        overrun: run.budget_overrun,
    }
}

pub(super) fn terminal_projection_output(projection: &TerminalProjection) -> Option<Value> {
    match projection {
        TerminalProjection::Completed { output } => Some(output.clone()),
        TerminalProjection::Partial { output, .. } | TerminalProjection::Blocked { output, .. } => {
            output.clone()
        }
        TerminalProjection::Unsupported { .. }
        | TerminalProjection::Failed { .. }
        | TerminalProjection::Cancelled { .. } => None,
    }
}

pub(super) fn scheduling_projection(
    run: &ExecutionRunRecord,
    tasks: &[ExecutionTaskRecord],
) -> ExecutionProjection {
    let task_projections = tasks
        .iter()
        .map(|task| ExecutionTaskProjection {
            task_id: task.task_id,
            node_id: task.node_id.clone(),
            item_key: task.item_key.clone(),
            status: task.status,
            attempt: task.attempt,
            generation: task.generation,
            input: task.input.clone(),
            outcome: task.current_outcome.clone(),
        })
        .collect::<Vec<_>>();
    let mut node_statuses = BTreeMap::new();
    for node in &run.active_plan.definition.nodes {
        let node_tasks = tasks
            .iter()
            .filter(|task| task.node_id == node.id)
            .collect::<Vec<_>>();
        let status = persisted_node_status(&node.operation, &node_tasks);
        node_statuses.insert(node.id.clone(), status);
    }
    ExecutionProjection {
        plan_revision: run.plan_revision,
        node_statuses,
        tasks: task_projections,
    }
}

pub(super) fn persisted_node_status(
    operation: &ExecutionOperation,
    tasks: &[&ExecutionTaskRecord],
) -> ExecutionNodeStatus {
    if tasks.is_empty() {
        return ExecutionNodeStatus::Pending;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::WaitingInput | ExecutionTaskStatus::WaitingReplan
        ) || (task.status == ExecutionTaskStatus::Running
            && matches!(
                task.kind,
                LogicalTaskKind::Review { .. } | LogicalTaskKind::WaitSignal { .. }
            ))
    }) {
        return ExecutionNodeStatus::Waiting;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::Pending
                | ExecutionTaskStatus::Reserved
                | ExecutionTaskStatus::Running
        )
    }) {
        return ExecutionNodeStatus::Running;
    }
    if tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::Failed)
    {
        return ExecutionNodeStatus::Failed;
    }
    if tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::Cancelled)
    {
        return ExecutionNodeStatus::Cancelled;
    }
    if matches!(
        operation,
        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. }
    ) {
        return ExecutionNodeStatus::Pending;
    }
    if tasks
        .iter()
        .all(|task| task.status == ExecutionTaskStatus::Skipped)
    {
        ExecutionNodeStatus::Skipped
    } else {
        ExecutionNodeStatus::Completed
    }
}
