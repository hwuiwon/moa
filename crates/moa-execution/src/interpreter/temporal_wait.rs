//! Pure projection and due-settlement decisions for storage-only execution waits.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    ExecutionOperation, ExecutionTaskResult, ExecutionTemporalTarget,
};
use serde_json::Value;

use super::ScheduleRequest;
use crate::{
    Result,
    bindings::{BindingContext, resolve_bindings},
    state::{ExecutionNodeStatus, ExecutionTaskStatus, WaitSettlement, WaitingReason},
};

pub(super) fn waiting_reasons(
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
                | ExecutionTaskStatus::Ready
                | ExecutionTaskStatus::Reserved
                | ExecutionTaskStatus::Dispatching
                | ExecutionTaskStatus::Running
                | ExecutionTaskStatus::WaitingReplan
        )
    }) {
        waiting.push(WaitingReason::RunningTasks);
    }
    for task in &request.projection.tasks {
        if task.status == ExecutionTaskStatus::WaitingExternal {
            waiting.push(WaitingReason::External {
                task_id: task.task_id,
            });
        }
        if task.status == ExecutionTaskStatus::WaitingInput
            && let Some(outcome) = &task.outcome
            && let ExecutionTaskResult::NeedsInput { question, audience } = &outcome.result
        {
            waiting.push(WaitingReason::Input {
                task_id: task.task_id,
                audience: audience.clone(),
                question: question.clone(),
                wait_policy: request.plan.definition.input_wait_policy.clone(),
            });
        }
        if request.projection.node_statuses.get(&task.node_id)
            == Some(&ExecutionNodeStatus::Waiting)
            && let Some(node) = by_id.get(task.node_id.as_str())
        {
            match &node.operation {
                ExecutionOperation::Review {
                    prompt,
                    wait_policy,
                } => waiting.push(WaitingReason::Review {
                    task_id: task.task_id,
                    prompt: prompt.clone(),
                    wait_policy: wait_policy.clone(),
                }),
                ExecutionOperation::WaitSignal {
                    signal_name,
                    wait_policy,
                } => {
                    waiting.push(WaitingReason::Signal {
                        task_id: task.task_id,
                        signal_name: signal_name.clone(),
                        wait_policy: wait_policy.clone(),
                    });
                }
                ExecutionOperation::WaitUntil { wake, .. } => {
                    waiting.push(WaitingReason::Timer {
                        task_id: task.task_id,
                        wake: wake.clone(),
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

pub(super) fn ready_wait_settlement(
    request: &ScheduleRequest,
    outputs: &BTreeMap<String, Value>,
) -> Result<Option<WaitSettlement>> {
    let by_id = request
        .plan
        .definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let mut settlements = Vec::new();
    for task in &request.projection.tasks {
        if task.status == ExecutionTaskStatus::WaitingInput
            && temporal_target_is_due(
                &request.plan.definition.input_wait_policy.expiry,
                request.now,
            )
        {
            settlements.push((
                task.task_id,
                WaitSettlement::WaitExpired {
                    task_id: task.task_id,
                    action: request.plan.definition.input_wait_policy.on_expiry.clone(),
                },
            ));
            continue;
        }
        let Some(node) = by_id.get(task.node_id.as_str()) else {
            continue;
        };
        let settlement = match &node.operation {
            ExecutionOperation::Review { wait_policy, .. }
                if matches!(
                    task.status,
                    ExecutionTaskStatus::Running | ExecutionTaskStatus::WaitingReview
                ) && temporal_target_is_due(&wait_policy.expiry, request.now) =>
            {
                Some(WaitSettlement::WaitExpired {
                    task_id: task.task_id,
                    action: wait_policy.on_expiry.clone(),
                })
            }
            ExecutionOperation::WaitSignal { wait_policy, .. }
                if matches!(
                    task.status,
                    ExecutionTaskStatus::Running | ExecutionTaskStatus::WaitingSignal
                ) && temporal_target_is_due(&wait_policy.expiry, request.now) =>
            {
                Some(WaitSettlement::WaitExpired {
                    task_id: task.task_id,
                    action: wait_policy.on_expiry.clone(),
                })
            }
            ExecutionOperation::WaitUntil { wake, result }
                if matches!(
                    task.status,
                    ExecutionTaskStatus::Running | ExecutionTaskStatus::WaitingTimer
                ) && temporal_target_is_due(wake, request.now) =>
            {
                let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
                let output = resolve_bindings(
                    result,
                    &BindingContext {
                        run_input: &request.run_input,
                        node_outputs: outputs,
                        dependencies: &dependencies,
                        item: None,
                        item_key: None,
                    },
                )?;
                Some(WaitSettlement::TimerElapsed {
                    task_id: task.task_id,
                    output,
                })
            }
            _ => None,
        };
        if let Some(settlement) = settlement {
            settlements.push((task.task_id, settlement));
        }
    }
    settlements.sort_by_key(|(task_id, _)| *task_id);
    Ok(settlements
        .into_iter()
        .next()
        .map(|(_, settlement)| settlement))
}

fn temporal_target_is_due(target: &ExecutionTemporalTarget, now: DateTime<Utc>) -> bool {
    matches!(target, ExecutionTemporalTarget::At { at } if now >= *at)
}
