//! Terminal completion and verifier scheduler decisions.

use super::temporal_wait::waiting_reasons;
use super::*;

pub(super) fn schedule_verifiers_or_complete(request: ScheduleRequest) -> Result<ScheduleDecision> {
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
            compensation: None,
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
                    | ExecutionTaskStatus::UnknownOutcome
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

pub(super) fn verifier_summaries(
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
                    ExecutionTaskResult::UnknownOutcome { message } => Some(ExecutionTaskFailure {
                        class: ExecutionFailureClass::Terminal,
                        message: message.clone(),
                        capability_ref: plan
                            .definition
                            .nodes
                            .iter()
                            .find(|node| node.id == task.node_id)
                            .and_then(|node| operation_capability(&node.operation)),
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

pub(super) fn budget_terminal(output: Option<Value>) -> ScheduleDecision {
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

pub(super) fn completion_terminal(
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
    let failure = (evaluation.status == CompletionStatus::Failed)
        .then(|| terminal_failure(request, &evaluation.gaps));
    let terminal = terminal_projection_from_evaluation(
        &evaluation,
        terminal_output,
        None,
        failure,
        (evaluation.status == CompletionStatus::Unsupported)
            .then(|| "required execution paths are unsupported".to_string()),
    )?;
    Ok(ScheduleDecision::Terminal(terminal))
}

pub(super) fn terminal_failure(request: &ScheduleRequest, gaps: &[String]) -> ExecutionTaskFailure {
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
                    ExecutionTaskResult::UnknownOutcome { message } => Some(ExecutionTaskFailure {
                        class: ExecutionFailureClass::Terminal,
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

pub(super) fn terminal_output(
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

pub(super) fn operation_capability(operation: &ExecutionOperation) -> Option<CapabilityReference> {
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
        | ExecutionOperation::WaitUntil { .. }
        | ExecutionOperation::Output { .. } => None,
    }
}

pub(super) fn effective_status(
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

pub(super) const fn is_terminal_node_status(status: ExecutionNodeStatus) -> bool {
    matches!(
        status,
        ExecutionNodeStatus::Completed
            | ExecutionNodeStatus::Skipped
            | ExecutionNodeStatus::Failed
            | ExecutionNodeStatus::Cancelled
    )
}

pub(super) const fn is_terminal_task_status(status: ExecutionTaskStatus) -> bool {
    matches!(
        status,
        ExecutionTaskStatus::Completed
            | ExecutionTaskStatus::Skipped
            | ExecutionTaskStatus::Failed
            | ExecutionTaskStatus::UnknownOutcome
            | ExecutionTaskStatus::Cancelled
    )
}
