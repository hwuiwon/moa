//! Exact wake claiming and bounded node-page advancement.

use std::collections::BTreeMap;

use super::{
    ExecutionRunAdvanceOutcome, ExecutionRunAdvanceRequest, ExecutionRunAdvanceResponse, settlement,
};
use chrono::{DateTime, Utc};
use moa_execution::{
    NodeMaterializationPage, ReduceMaterializationPageInput, ScheduleRequest,
    budget::BudgetLedger,
    materialize_node_page,
    repository::{
        ExecutionRepository, ExecutionScope, RunControllerClaimOutcome,
        RunControllerCompletionOutcome, RunControllerCompletionRequest, RunDeadlineArmOutcome,
        completion::{CompletionAdvanceOutcome, CompletionAdvanceRequest},
        outbox::{ExecutionDispatchKind, ExecutionDispatchRecord},
        ready::{
            ExecutionReduceMaterializationCursor, MapAggregatePageOutcome, MapAggregatePageRequest,
            ReadyMaterializationOutcome, ReadyMaterializationRequest, ReduceRoundInputPageRequest,
        },
        terminal::{
            FinalizationOutcome, PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage,
            RunTriggerDrainOutcome, RunTriggerDrainRequest,
        },
    },
    state::{ExecutionProjection, ExecutionTaskStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Journaled database commit and the bounded side effects selected by it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControllerAdvanceCommit {
    pub(super) response: ExecutionRunAdvanceResponse,
    pub(super) publish_progress: bool,
    pub(super) terminal_delivery: Option<moa_execution::wire::ExecutionTerminalDelivery>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActivationLimits {
    remaining_steps: usize,
    remaining_tasks: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationPreflight {
    PendingTerminal,
    DueDeadline,
    Ordinary,
}

fn completion_scan_steps(scanned_tasks: u32, scanned_nodes: u32) -> moa_execution::Result<usize> {
    usize::try_from(scanned_tasks)
        .ok()
        .and_then(|tasks| {
            usize::try_from(scanned_nodes)
                .ok()
                .and_then(|nodes| tasks.checked_add(nodes))
        })
        .ok_or_else(|| moa_execution::Error::ArithmeticOverflow {
            context: "controller completion scan count".to_string(),
        })
}

fn terminal_trigger_page_limit(remaining_steps: usize) -> moa_execution::Result<Option<u32>> {
    if remaining_steps == 0 {
        return Ok(None);
    }
    u32::try_from(remaining_steps.min(1_000))
        .map(Some)
        .map_err(|_| moa_execution::Error::ArithmeticOverflow {
            context: "controller terminal trigger-drain page limit".to_string(),
        })
}

fn pending_terminal_step_count(
    settled_task_count: u64,
    drained_trigger_count: u32,
    cancellation_dispatch_count: usize,
    compensation_admitted: bool,
) -> moa_execution::Result<usize> {
    usize::try_from(settled_task_count)
        .ok()
        .and_then(|settled| {
            usize::try_from(drained_trigger_count)
                .ok()
                .and_then(|drained| settled.checked_add(drained))
        })
        .and_then(|count| count.checked_add(cancellation_dispatch_count))
        .and_then(|count| count.checked_add(usize::from(compensation_admitted)))
        .ok_or_else(|| moa_execution::Error::ArithmeticOverflow {
            context: "controller pending-terminal step count".to_string(),
        })
}

fn map_aggregate_requires_continuation(
    outcome: &MapAggregatePageOutcome,
) -> moa_execution::Result<bool> {
    match outcome {
        MapAggregatePageOutcome::Applied {
            aggregate_complete, ..
        }
        | MapAggregatePageOutcome::Replayed {
            aggregate_complete, ..
        } => Ok(!aggregate_complete),
        MapAggregatePageOutcome::Overflow | MapAggregatePageOutcome::Conflict => Ok(true),
        MapAggregatePageOutcome::NotFound => Err(moa_execution::Error::InvalidRepositoryData {
            message: "execution run disappeared during bounded map aggregation".to_string(),
        }),
    }
}

fn validate_resumed_recovery_commit(
    prior_wake_epoch: u64,
    current_wake_epoch: u64,
    continuation_enqueued: bool,
) -> moa_execution::Result<()> {
    let expected_wake_epoch = prior_wake_epoch.checked_add(1).ok_or_else(|| {
        moa_execution::Error::ArithmeticOverflow {
            context: "resumed activation continuation wake epoch".to_string(),
        }
    })?;
    if !continuation_enqueued || current_wake_epoch != expected_wake_epoch {
        return Err(moa_execution::Error::InvalidRepositoryData {
            message: "resumed activation recovery must enqueue exactly one fresh wake".to_string(),
        });
    }
    Ok(())
}

fn validate_trigger_drain_continuation(
    prior_wake_epoch: u64,
    current_wake_epoch: u64,
    drained_trigger_count: u32,
) -> moa_execution::Result<()> {
    let expected_wake_epoch = prior_wake_epoch.checked_add(1).ok_or_else(|| {
        moa_execution::Error::ArithmeticOverflow {
            context: "terminal trigger-drain continuation wake epoch".to_string(),
        }
    })?;
    if drained_trigger_count == 0 || current_wake_epoch != expected_wake_epoch {
        return Err(moa_execution::Error::InvalidRepositoryData {
            message:
                "terminal trigger drain must settle a nonempty page and enqueue one fresh wake"
                    .to_string(),
        });
    }
    Ok(())
}

fn validate_replan_stop_continuation(
    run: &moa_execution::repository::ExecutionRunRecord,
    continuation: &ExecutionDispatchRecord,
    continuation_wake_epoch: u64,
) -> moa_execution::Result<()> {
    let exact_owner = continuation.kind == ExecutionDispatchKind::RunActivation
        && continuation.tenant_id == run.tenant_id
        && continuation.run_uid == Some(run.run_uid)
        && continuation.controller_generation == Some(run.controller_generation)
        && continuation.wake_epoch == Some(continuation_wake_epoch);
    validate_replan_stop_continuation_fields(run.wake_epoch, continuation_wake_epoch, exact_owner)
}

fn validate_replan_stop_continuation_fields(
    prior_wake_epoch: u64,
    continuation_wake_epoch: u64,
    exact_owner: bool,
) -> moa_execution::Result<()> {
    validate_resumed_recovery_commit(prior_wake_epoch, continuation_wake_epoch, true)?;
    if !exact_owner {
        return Err(moa_execution::Error::InvalidRepositoryData {
            message: "replan-stop continuation does not own the exact fresh run wake".to_string(),
        });
    }
    Ok(())
}

fn activation_preflight(
    has_pending_terminal: bool,
    deadline_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> ActivationPreflight {
    if has_pending_terminal {
        ActivationPreflight::PendingTerminal
    } else if deadline_at.is_some_and(|deadline_at| deadline_at <= now) {
        ActivationPreflight::DueDeadline
    } else {
        ActivationPreflight::Ordinary
    }
}

impl ActivationLimits {
    fn new(maximum_steps: usize, dispatch_batch_size: usize) -> moa_execution::Result<Self> {
        if maximum_steps == 0 || dispatch_batch_size == 0 {
            return Err(moa_execution::Error::InvalidRepositoryInput {
                message: "controller activation bounds must both be greater than zero".to_string(),
            });
        }
        Ok(Self {
            remaining_steps: maximum_steps,
            remaining_tasks: dispatch_batch_size,
        })
    }

    fn inspect_nodes(&mut self, count: usize) -> usize {
        let inspected = self.remaining_steps.min(count);
        self.remaining_steps -= inspected;
        inspected
    }

    fn record_steps(&mut self, count: usize) -> moa_execution::Result<()> {
        self.remaining_steps = self.remaining_steps.checked_sub(count).ok_or_else(|| {
            moa_execution::Error::InvalidRepositoryData {
                message: "controller completion scan exceeded its activation bound".to_string(),
            }
        })?;
        Ok(())
    }

    fn task_page_limit(&self) -> moa_execution::Result<u32> {
        u32::try_from(self.remaining_tasks.min(1_000)).map_err(|_| {
            moa_execution::Error::ArithmeticOverflow {
                context: "controller task page limit".to_string(),
            }
        })
    }

    fn record_tasks(&mut self, count: usize) -> moa_execution::Result<()> {
        self.remaining_tasks = self.remaining_tasks.checked_sub(count).ok_or_else(|| {
            moa_execution::Error::InvalidRepositoryData {
                message: "controller materialized beyond its dispatch bound".to_string(),
            }
        })?;
        Ok(())
    }
}

pub(super) fn validate_request(
    object_key: &str,
    request: &ExecutionRunAdvanceRequest,
) -> Result<(), restate_sdk::prelude::HandlerError> {
    if request.dispatch_uid.is_nil() || request.run_uid.is_nil() {
        return Err(crate::workflows::errors::bad_request(
            "execution controller identifiers must not be nil",
        ));
    }
    if object_key != request.run_uid.to_string() {
        return Err(crate::workflows::errors::bad_request(
            "execution controller key does not match run_uid",
        ));
    }
    Ok(())
}

pub(super) async fn advance(
    repository: ExecutionRepository,
    config: moa_config::ExecutionConfig,
    request: ExecutionRunAdvanceRequest,
) -> moa_execution::Result<ControllerAdvanceCommit> {
    let Some(admitted) = repository
        .load_run(ExecutionScope::ControlPlane, request.run_uid)
        .await?
    else {
        return Err(moa_execution::Error::InvalidRepositoryInput {
            message: "execution run not found".to_string(),
        });
    };
    if admitted.tenant_id != request.tenant_id
        || admitted.admitted_identity.tenant_id != request.tenant_id
    {
        return Err(moa_execution::Error::InvalidRepositoryInput {
            message: "activation tenant does not own the admitted execution run".to_string(),
        });
    }
    let scope = admitted.contact_id.map_or(
        ExecutionScope::Tenant {
            tenant_id: admitted.tenant_id,
        },
        |contact_id| ExecutionScope::Contact {
            tenant_id: admitted.tenant_id,
            contact_id,
        },
    );
    let claim = repository
        .claim_controller_wake(
            scope,
            request.run_uid,
            request.controller_generation,
            request.wake_epoch,
        )
        .await?;
    let run = match claim {
        RunControllerClaimOutcome::Claimed(run) => run,
        RunControllerClaimOutcome::Resumed(run) => {
            return resume_with_bounded_continuation(&repository, scope, &config, &request, &run)
                .await;
        }
        RunControllerClaimOutcome::Replayed(run) => {
            return Ok(noop_commit(ExecutionRunAdvanceOutcome::Replayed, &run));
        }
        RunControllerClaimOutcome::Terminal(run) => {
            return terminal_commit(&repository, scope, &run).await;
        }
        RunControllerClaimOutcome::StaleGeneration { current_generation } => {
            return Ok(stale_commit(current_generation, admitted.wake_epoch));
        }
        RunControllerClaimOutcome::StaleWake {
            current_wake_epoch, ..
        } => {
            return Ok(stale_commit(
                admitted.controller_generation,
                current_wake_epoch,
            ));
        }
        RunControllerClaimOutcome::NotFound => {
            return Err(moa_execution::Error::InvalidRepositoryInput {
                message: "execution run disappeared during activation claim".to_string(),
            });
        }
        RunControllerClaimOutcome::InvalidState => {
            return Ok(stale_commit(
                admitted.controller_generation,
                admitted.wake_epoch,
            ));
        }
    };

    let mut limits =
        ActivationLimits::new(config.maximum_activation_steps, config.dispatch_batch_size)?;
    let now = Utc::now();
    let terminal_page_limit = u32::try_from(
        limits
            .remaining_steps
            .min(limits.remaining_tasks)
            .min(1_000),
    )
    .map_err(|_| moa_execution::Error::ArithmeticOverflow {
        context: "controller pending-terminal page limit".to_string(),
    })?;
    match activation_preflight(
        run.pending_terminal.is_some(),
        run.approved_budget.deadline_at,
        now,
    ) {
        ActivationPreflight::PendingTerminal => {
            let outcome = repository
                .advance_pending_terminal_settlement(
                    &config,
                    scope,
                    run.run_uid,
                    run.controller_generation,
                    run.wake_epoch,
                    now,
                    terminal_page_limit,
                )
                .await?;
            return pending_terminal_commit(&repository, scope, &run, outcome).await;
        }
        ActivationPreflight::DueDeadline => {
            let outcome = repository
                .fence_deadline_and_enqueue_settlement(
                    &config,
                    scope,
                    run.run_uid,
                    run.controller_generation,
                    run.wake_epoch,
                    now,
                    terminal_page_limit,
                )
                .await?;
            return pending_terminal_commit(&repository, scope, &run, outcome).await;
        }
        ActivationPreflight::Ordinary => {}
    }

    match repository
        .arm_run_deadline(scope, run.run_uid, run.controller_generation, &config)
        .await?
    {
        RunDeadlineArmOutcome::Armed(_)
        | RunDeadlineArmOutcome::NoDeadline
        | RunDeadlineArmOutcome::Terminal => {}
        RunDeadlineArmOutcome::NotFound => {
            return Err(moa_execution::Error::InvalidRepositoryInput {
                message: "execution run disappeared while arming deadline".to_string(),
            });
        }
        RunDeadlineArmOutcome::StaleGeneration { .. } => {
            return Ok(stale_commit(run.controller_generation, run.wake_epoch));
        }
    }

    let mut activation_steps = 0usize;
    let mut materialized_tasks = 0usize;
    let mut bounded_work_remains = false;

    if let Some(intent) = repository
        .load_replan_stop_intent(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        )
        .await?
    {
        let page_size = u32::try_from(limits.remaining_steps.min(1_000)).map_err(|_| {
            moa_execution::Error::ArithmeticOverflow {
                context: "controller replan-stop completion page limit".to_string(),
            }
        })?;
        let completion = repository
            .advance_replan_stop_completion_projection(
                scope,
                &config,
                CompletionAdvanceRequest {
                    run_uid: run.run_uid,
                    controller_generation: run.controller_generation,
                    wake_epoch: run.wake_epoch,
                    page_size,
                    now,
                },
                &intent,
            )
            .await?;
        match completion {
            CompletionAdvanceOutcome::ReplanStopContinue {
                scanned_tasks,
                scanned_nodes,
                continuation,
            } => {
                let scanned = completion_scan_steps(scanned_tasks, scanned_nodes)?;
                if scanned == 0 {
                    return Err(moa_execution::Error::InvalidRepositoryData {
                        message: "replan-stop continuation made no bounded progress".to_string(),
                    });
                }
                limits.record_steps(scanned)?;
                activation_steps = activation_steps.checked_add(scanned).ok_or_else(|| {
                    moa_execution::Error::ArithmeticOverflow {
                        context: "controller activation step count".to_string(),
                    }
                })?;
                let continuation_wake_epoch = continuation.wake_epoch.ok_or_else(|| {
                    moa_execution::Error::InvalidRepositoryData {
                        message: "replan-stop continuation is missing its wake epoch".to_string(),
                    }
                })?;
                validate_replan_stop_continuation(&run, &continuation, continuation_wake_epoch)?;
                return Ok(ControllerAdvanceCommit {
                    response: ExecutionRunAdvanceResponse {
                        outcome: ExecutionRunAdvanceOutcome::Advanced,
                        controller_generation: run.controller_generation,
                        wake_epoch: continuation_wake_epoch,
                        activation_steps,
                        materialized_tasks: 0,
                        continuation_enqueued: true,
                    },
                    publish_progress: true,
                    terminal_delivery: None,
                });
            }
            CompletionAdvanceOutcome::ReplanStopReady {
                pending_terminal,
                receipt,
            } => {
                let outcome = repository
                    .fence_replan_stop_and_enqueue_settlement(
                        &config,
                        scope,
                        run.run_uid,
                        run.controller_generation,
                        run.plan_revision,
                        run.wake_epoch,
                        pending_terminal,
                        receipt,
                        now,
                        terminal_page_limit,
                    )
                    .await?;
                return pending_terminal_commit(&repository, scope, &run, outcome).await;
            }
            CompletionAdvanceOutcome::NotReady => {
                return Ok(stale_commit(run.controller_generation, run.wake_epoch));
            }
            CompletionAdvanceOutcome::Continue { .. }
            | CompletionAdvanceOutcome::VerifiersMaterialized { .. }
            | CompletionAdvanceOutcome::WaitingForVerifiers
            | CompletionAdvanceOutcome::FinalizationReady(_)
            | CompletionAdvanceOutcome::NonSuccessTerminal { .. } => {
                return Err(moa_execution::Error::InvalidRepositoryData {
                    message: "replan-stop completion returned an ordinary completion outcome"
                        .to_string(),
                });
            }
        }
    }

    loop {
        if limits.remaining_steps == 0 {
            bounded_work_remains = true;
            break;
        }
        let Some(candidate) = repository
            .load_map_aggregate_candidate(
                scope,
                run.run_uid,
                run.controller_generation,
                run.wake_epoch,
            )
            .await?
        else {
            break;
        };
        let outcome = repository
            .advance_map_aggregate_page(
                scope,
                MapAggregatePageRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    controller_generation: run.controller_generation,
                    wake_epoch: run.wake_epoch,
                    node_id: candidate.node_id,
                    expected_cursor_item_key: candidate.cursor_item_key,
                },
            )
            .await?;
        limits.record_steps(1)?;
        activation_steps = activation_steps.checked_add(1).ok_or_else(|| {
            moa_execution::Error::ArithmeticOverflow {
                context: "controller activation step count".to_string(),
            }
        })?;
        if map_aggregate_requires_continuation(&outcome)? {
            bounded_work_remains = true;
            break;
        }
    }

    'pages: while !bounded_work_remains {
        if limits.remaining_steps == 0 {
            bounded_work_remains = true;
            break;
        }
        let page_limit = u32::try_from(limits.remaining_steps.min(1_000)).map_err(|_| {
            moa_execution::Error::ArithmeticOverflow {
                context: "controller activation node page limit".to_string(),
            }
        })?;
        let Some(projection) = repository
            .load_activation_projection(scope, run.run_uid, page_limit)
            .await?
        else {
            return Err(moa_execution::Error::InvalidRepositoryData {
                message: "claimed execution run has no activation projection".to_string(),
            });
        };
        if projection.nodes.is_empty() {
            break;
        }
        let inspected = limits.inspect_nodes(projection.nodes.len());
        activation_steps = activation_steps.checked_add(inspected).ok_or_else(|| {
            moa_execution::Error::ArithmeticOverflow {
                context: "controller activation step count".to_string(),
            }
        })?;
        for node in projection.nodes.iter().take(inspected) {
            if limits.remaining_tasks == 0 {
                bounded_work_remains = true;
                break 'pages;
            }
            let schedule = ScheduleRequest {
                run_uid: projection.run.run_uid,
                goal: projection.run.goal.clone(),
                plan: projection.run.active_plan.clone(),
                catalog: projection.run.catalog.clone(),
                run_input: projection.run.input.clone(),
                projection: ExecutionProjection {
                    plan_revision: projection.run.plan_revision,
                    node_statuses: BTreeMap::new(),
                    tasks: Vec::new(),
                },
                config: config.clone(),
                budget_ledger: BudgetLedger {
                    limit: projection.run.approved_budget.clone(),
                    reserved: projection.run.reserved,
                    consumed: projection.run.consumed,
                    overrun: projection.run.budget_overrun,
                },
                now,
            };
            let task_page_limit = limits.task_page_limit()?;
            let plan_node = schedule
                .plan
                .definition
                .nodes
                .iter()
                .find(|plan_node| plan_node.id == node.node_id)
                .ok_or_else(|| moa_execution::Error::InvalidRepositoryData {
                    message: format!(
                        "activation node `{}` is missing from the active plan",
                        node.node_id
                    ),
                })?;
            let reduce_input = match &plan_node.operation {
                moa_artifacts::execution_plan::ExecutionOperation::Reduce {
                    batch_size, ..
                } => {
                    let page_inputs = if node.reduce_round == 1 {
                        Vec::new()
                    } else {
                        let round_input_count = node.reduce_round_input_count.ok_or_else(|| {
                            moa_execution::Error::InvalidRepositoryData {
                                message: format!(
                                    "reduce node `{}` round {} is missing its input count",
                                    node.node_id, node.reduce_round
                                ),
                            }
                        })?;
                        repository
                            .load_reduce_round_inputs(
                                scope,
                                ReduceRoundInputPageRequest {
                                    run_uid: run.run_uid,
                                    node_id: node.node_id.clone(),
                                    source_round: node.reduce_round.checked_sub(1).ok_or_else(
                                        || moa_execution::Error::ArithmeticOverflow {
                                            context: format!(
                                                "reduce node {} source round",
                                                node.node_id
                                            ),
                                        },
                                    )?,
                                    cursor: ExecutionReduceMaterializationCursor {
                                        round: node.reduce_round,
                                        batch_cursor: node.reduce_batch_cursor,
                                        round_input_count,
                                    },
                                    batch_size: *batch_size,
                                    target_batch_limit: task_page_limit,
                                },
                            )
                            .await?
                    };
                    Some(ReduceMaterializationPageInput {
                        round: node.reduce_round,
                        batch_cursor: node.reduce_batch_cursor,
                        round_input_count: node.reduce_round_input_count,
                        page_inputs,
                    })
                }
                _ => None,
            };
            let NodeMaterializationPage {
                tasks,
                source_exhausted,
                reduce_cursor,
                terminal_output,
                ..
            } = materialize_node_page(
                &schedule,
                &node.node_id,
                &projection.referenced_outputs,
                node.materialization_cursor,
                task_page_limit,
                reduce_input.as_ref(),
            )?;
            let task_count = tasks.len();
            match repository
                .materialize_ready_page(
                    scope,
                    &config,
                    ReadyMaterializationRequest {
                        run_uid: run.run_uid,
                        plan_revision: run.plan_revision,
                        node_id: node.node_id.clone(),
                        expected_cursor: node.materialization_cursor,
                        reduce_cursor: reduce_cursor.map(|cursor| {
                            ExecutionReduceMaterializationCursor {
                                round: cursor.round,
                                batch_cursor: cursor.batch_cursor,
                                round_input_count: cursor.round_input_count,
                            }
                        }),
                        source_exhausted,
                        terminal_output,
                        tasks,
                    },
                )
                .await?
            {
                ReadyMaterializationOutcome::Applied { tasks, .. }
                | ReadyMaterializationOutcome::Replayed { tasks, .. } => {
                    if tasks.len() != task_count {
                        return Err(moa_execution::Error::InvalidRepositoryData {
                            message: "ready materialization returned a different task page"
                                .to_string(),
                        });
                    }
                    limits.record_tasks(task_count)?;
                    materialized_tasks =
                        materialized_tasks.checked_add(task_count).ok_or_else(|| {
                            moa_execution::Error::ArithmeticOverflow {
                                context: "controller materialized task count".to_string(),
                            }
                        })?;
                }
                ReadyMaterializationOutcome::Conflict => {
                    bounded_work_remains = true;
                    break 'pages;
                }
            }
            bounded_work_remains |= !source_exhausted;
            if limits.remaining_tasks == 0 {
                break 'pages;
            }
        }
        if inspected < projection.nodes.len()
            || projection.has_more_actionable
            || limits.remaining_steps == 0
        {
            bounded_work_remains = true;
            break;
        }
    }

    let readiness = repository
        .load_activation_readiness(scope, run.run_uid)
        .await?
        .ok_or_else(|| moa_execution::Error::InvalidRepositoryData {
            message: "execution run disappeared while loading activation readiness".to_string(),
        })?;
    bounded_work_remains |= readiness.has_actionable_nodes;

    if readiness.terminal_ready() && !bounded_work_remains {
        if limits.remaining_steps == 0 || limits.remaining_tasks == 0 {
            bounded_work_remains = true;
        } else {
            let completion_page_limit = u32::try_from(
                limits
                    .remaining_steps
                    .min(limits.remaining_tasks)
                    .min(1_000),
            )
            .map_err(|_| moa_execution::Error::ArithmeticOverflow {
                context: "controller completion page limit".to_string(),
            })?;
            match repository
                .advance_completion_projection(
                    scope,
                    &config,
                    CompletionAdvanceRequest {
                        run_uid: run.run_uid,
                        controller_generation: request.controller_generation,
                        wake_epoch: request.wake_epoch,
                        page_size: completion_page_limit,
                        now,
                    },
                )
                .await?
            {
                CompletionAdvanceOutcome::Continue {
                    scanned_tasks,
                    scanned_nodes,
                } => {
                    let scanned = completion_scan_steps(scanned_tasks, scanned_nodes)?;
                    if scanned == 0 {
                        return Err(moa_execution::Error::InvalidRepositoryData {
                            message: "completion continuation made no bounded progress".to_string(),
                        });
                    }
                    limits.record_steps(scanned)?;
                    activation_steps = activation_steps.checked_add(scanned).ok_or_else(|| {
                        moa_execution::Error::ArithmeticOverflow {
                            context: "controller activation step count".to_string(),
                        }
                    })?;
                    bounded_work_remains = true;
                }
                CompletionAdvanceOutcome::ReplanStopContinue { .. }
                | CompletionAdvanceOutcome::ReplanStopReady { .. } => {
                    return Err(moa_execution::Error::InvalidRepositoryData {
                        message: "ordinary completion returned a replan-stop outcome".to_string(),
                    });
                }
                CompletionAdvanceOutcome::VerifiersMaterialized { tasks } => {
                    if tasks.is_empty()
                        || tasks
                            .iter()
                            .any(|task| task.status != ExecutionTaskStatus::Ready)
                    {
                        return Err(moa_execution::Error::InvalidRepositoryData {
                            message:
                                "completion verifier page was empty or contained non-ready work"
                                    .to_string(),
                        });
                    }
                    let task_count = tasks.len();
                    limits.record_steps(task_count)?;
                    limits.record_tasks(task_count)?;
                    activation_steps =
                        activation_steps.checked_add(task_count).ok_or_else(|| {
                            moa_execution::Error::ArithmeticOverflow {
                                context: "controller activation step count".to_string(),
                            }
                        })?;
                    materialized_tasks =
                        materialized_tasks.checked_add(task_count).ok_or_else(|| {
                            moa_execution::Error::ArithmeticOverflow {
                                context: "controller materialized task count".to_string(),
                            }
                        })?;
                    bounded_work_remains = true;
                }
                CompletionAdvanceOutcome::WaitingForVerifiers => {}
                CompletionAdvanceOutcome::FinalizationReady(finalization) => {
                    if let Some(trigger_page_limit) =
                        terminal_trigger_page_limit(limits.remaining_steps)?
                    {
                        let mut may_finalize = true;
                        match repository
                            .drain_run_triggers_page(
                                scope,
                                &config,
                                RunTriggerDrainRequest {
                                    run_uid: run.run_uid,
                                    controller_generation: run.controller_generation,
                                    wake_epoch: run.wake_epoch,
                                    page_limit: trigger_page_limit,
                                    now,
                                },
                            )
                            .await?
                        {
                            RunTriggerDrainOutcome::PageDrained(commit) => {
                                let drained = usize::try_from(commit.drained_trigger_count)
                                    .map_err(|_| moa_execution::Error::ArithmeticOverflow {
                                        context: "controller drained trigger count".to_string(),
                                    })?;
                                limits.record_steps(drained)?;
                                activation_steps = activation_steps
                                    .checked_add(drained)
                                    .ok_or_else(|| moa_execution::Error::ArithmeticOverflow {
                                        context: "controller activation step count".to_string(),
                                    })?;
                                validate_trigger_drain_continuation(
                                    run.wake_epoch,
                                    commit.run.wake_epoch,
                                    commit.drained_trigger_count,
                                )?;
                                return Ok(ControllerAdvanceCommit {
                                    response: ExecutionRunAdvanceResponse {
                                        outcome: ExecutionRunAdvanceOutcome::Advanced,
                                        controller_generation: commit.run.controller_generation,
                                        wake_epoch: commit.run.wake_epoch,
                                        activation_steps,
                                        materialized_tasks,
                                        continuation_enqueued: true,
                                    },
                                    publish_progress: true,
                                    terminal_delivery: None,
                                });
                            }
                            RunTriggerDrainOutcome::ReadyToFinalize {
                                run: drained_run,
                                drained_trigger_count,
                            } => {
                                let drained =
                                    usize::try_from(drained_trigger_count).map_err(|_| {
                                        moa_execution::Error::ArithmeticOverflow {
                                            context: "controller drained trigger count".to_string(),
                                        }
                                    })?;
                                limits.record_steps(drained)?;
                                activation_steps = activation_steps
                                    .checked_add(drained)
                                    .ok_or_else(|| moa_execution::Error::ArithmeticOverflow {
                                        context: "controller activation step count".to_string(),
                                    })?;
                                if drained_run.controller_generation != run.controller_generation
                                    || drained_run.wake_epoch != run.wake_epoch
                                {
                                    return Err(moa_execution::Error::InvalidRepositoryData {
                                        message: "terminal trigger drain changed the claimed wake"
                                            .to_string(),
                                    });
                                }
                                if limits.remaining_steps == 0 {
                                    bounded_work_remains = true;
                                    may_finalize = false;
                                } else {
                                    limits.record_steps(1)?;
                                    activation_steps =
                                        activation_steps.checked_add(1).ok_or_else(|| {
                                            moa_execution::Error::ArithmeticOverflow {
                                                context: "controller activation step count"
                                                    .to_string(),
                                            }
                                        })?;
                                }
                            }
                            RunTriggerDrainOutcome::Replayed(replayed) => {
                                return Ok(noop_commit(
                                    ExecutionRunAdvanceOutcome::Replayed,
                                    &replayed,
                                ));
                            }
                            RunTriggerDrainOutcome::StaleGeneration { current_generation } => {
                                return Ok(stale_commit(current_generation, run.wake_epoch));
                            }
                            RunTriggerDrainOutcome::StaleWake {
                                current_wake_epoch, ..
                            } => {
                                return Ok(stale_commit(
                                    run.controller_generation,
                                    current_wake_epoch,
                                ));
                            }
                            RunTriggerDrainOutcome::NotFound => {
                                return Err(moa_execution::Error::InvalidRepositoryData {
                                    message:
                                        "execution run disappeared during terminal trigger drain"
                                            .to_string(),
                                });
                            }
                            RunTriggerDrainOutcome::InvalidState => {
                                return Ok(stale_commit(run.controller_generation, run.wake_epoch));
                            }
                        }
                        if may_finalize {
                            match repository.finalize_run(scope, *finalization).await? {
                                FinalizationOutcome::Finalized(terminal)
                                | FinalizationOutcome::Replayed(terminal) => {
                                    let mut commit =
                                        terminal_commit(&repository, scope, &terminal).await?;
                                    commit.response.activation_steps = activation_steps;
                                    commit.response.materialized_tasks = materialized_tasks;
                                    return Ok(commit);
                                }
                                FinalizationOutcome::Conflict => bounded_work_remains = true,
                                FinalizationOutcome::NotFound => {
                                    return Err(moa_execution::Error::InvalidRepositoryData {
                                        message: "execution run disappeared during finalization"
                                            .to_string(),
                                    });
                                }
                            }
                        }
                    } else {
                        bounded_work_remains = true;
                    }
                }
                CompletionAdvanceOutcome::NonSuccessTerminal { pending_terminal } => {
                    let outcome = repository
                        .fence_completion_terminal_and_enqueue_settlement(
                            &config,
                            scope,
                            run.run_uid,
                            run.controller_generation,
                            run.wake_epoch,
                            pending_terminal,
                            now,
                            completion_page_limit,
                        )
                        .await?;
                    let mut commit =
                        pending_terminal_commit(&repository, scope, &run, outcome).await?;
                    commit.response.activation_steps = commit
                        .response
                        .activation_steps
                        .checked_add(activation_steps)
                        .ok_or_else(|| moa_execution::Error::ArithmeticOverflow {
                            context: "controller terminal activation step count".to_string(),
                        })?;
                    commit.response.materialized_tasks = materialized_tasks;
                    return Ok(commit);
                }
                CompletionAdvanceOutcome::NotReady => bounded_work_remains = true,
            }
        }
    }

    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .ok_or_else(|| moa_execution::Error::InvalidRepositoryData {
            message: "execution run disappeared before controller checkpoint".to_string(),
        })?;
    let checkpoint = if bounded_work_remains {
        settlement::continuation_checkpoint(&current)
    } else {
        settlement::parked_checkpoint(&current, now)
    };
    let completion = repository
        .complete_controller_wake(
            scope,
            &config,
            run.run_uid,
            RunControllerCompletionRequest {
                controller_generation: request.controller_generation,
                wake_epoch: request.wake_epoch,
                checkpoint,
                continuation_payload: bounded_work_remains.then(|| {
                    json!({
                        "cause": "bounded_controller_continuation",
                        "prior_dispatch_uid": request.dispatch_uid,
                    })
                }),
                continuation_not_before_at: now,
            },
        )
        .await?;
    let (current, continuation_enqueued, outcome) = match completion {
        RunControllerCompletionOutcome::Applied { run, continuation } => {
            let enqueued = continuation.is_some();
            (*run, enqueued, ExecutionRunAdvanceOutcome::Advanced)
        }
        RunControllerCompletionOutcome::Replayed(run) => {
            (*run, false, ExecutionRunAdvanceOutcome::Replayed)
        }
        RunControllerCompletionOutcome::CapacitySaturated { dimension } => {
            return Err(moa_execution::Error::CapacitySaturated {
                dimension: dimension.as_str(),
            });
        }
        RunControllerCompletionOutcome::StaleGeneration { current_generation } => {
            return Ok(stale_commit(current_generation, current.wake_epoch));
        }
        RunControllerCompletionOutcome::StaleWake {
            current_wake_epoch, ..
        } => {
            return Ok(stale_commit(
                current.controller_generation,
                current_wake_epoch,
            ));
        }
        RunControllerCompletionOutcome::NotFound => {
            return Err(moa_execution::Error::InvalidRepositoryData {
                message: "execution run disappeared during controller completion".to_string(),
            });
        }
        RunControllerCompletionOutcome::InvalidState => {
            return Ok(stale_commit(
                current.controller_generation,
                current.wake_epoch,
            ));
        }
    };
    Ok(ControllerAdvanceCommit {
        response: ExecutionRunAdvanceResponse {
            outcome,
            controller_generation: current.controller_generation,
            wake_epoch: current.wake_epoch,
            activation_steps,
            materialized_tasks,
            continuation_enqueued,
        },
        publish_progress: true,
        terminal_delivery: None,
    })
}

async fn resume_with_bounded_continuation(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    config: &moa_config::ExecutionConfig,
    request: &ExecutionRunAdvanceRequest,
    run: &moa_execution::repository::ExecutionRunRecord,
) -> moa_execution::Result<ControllerAdvanceCommit> {
    let completion = repository
        .complete_controller_wake(
            scope,
            config,
            run.run_uid,
            RunControllerCompletionRequest {
                controller_generation: request.controller_generation,
                wake_epoch: request.wake_epoch,
                checkpoint: settlement::continuation_checkpoint(run),
                continuation_payload: Some(json!({
                    "cause": "resumed_activation_recovery",
                    "prior_dispatch_uid": request.dispatch_uid,
                })),
                continuation_not_before_at: Utc::now(),
            },
        )
        .await?;
    match completion {
        RunControllerCompletionOutcome::Applied { run, continuation } => {
            let continuation_enqueued = continuation.is_some();
            validate_resumed_recovery_commit(
                request.wake_epoch,
                run.wake_epoch,
                continuation_enqueued,
            )?;
            Ok(ControllerAdvanceCommit {
                response: ExecutionRunAdvanceResponse {
                    outcome: ExecutionRunAdvanceOutcome::Advanced,
                    controller_generation: run.controller_generation,
                    wake_epoch: run.wake_epoch,
                    activation_steps: 0,
                    materialized_tasks: 0,
                    continuation_enqueued,
                },
                publish_progress: true,
                terminal_delivery: None,
            })
        }
        RunControllerCompletionOutcome::Replayed(run) => {
            Ok(noop_commit(ExecutionRunAdvanceOutcome::Replayed, &run))
        }
        RunControllerCompletionOutcome::CapacitySaturated { dimension } => {
            Err(moa_execution::Error::CapacitySaturated {
                dimension: dimension.as_str(),
            })
        }
        RunControllerCompletionOutcome::StaleGeneration { current_generation } => {
            Ok(stale_commit(current_generation, run.wake_epoch))
        }
        RunControllerCompletionOutcome::StaleWake {
            current_wake_epoch, ..
        } => Ok(stale_commit(run.controller_generation, current_wake_epoch)),
        RunControllerCompletionOutcome::NotFound => {
            Err(moa_execution::Error::InvalidRepositoryData {
                message: "execution run disappeared during resumed activation recovery".to_string(),
            })
        }
        RunControllerCompletionOutcome::InvalidState => {
            Ok(stale_commit(run.controller_generation, run.wake_epoch))
        }
    }
}

async fn pending_terminal_commit(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    claimed_run: &moa_execution::repository::ExecutionRunRecord,
    outcome: PendingTerminalAdvanceOutcome,
) -> moa_execution::Result<ControllerAdvanceCommit> {
    let (commit, response_outcome) = match outcome {
        PendingTerminalAdvanceOutcome::Applied(commit) => {
            (commit, ExecutionRunAdvanceOutcome::Advanced)
        }
        PendingTerminalAdvanceOutcome::Replayed(commit) => {
            (commit, ExecutionRunAdvanceOutcome::Replayed)
        }
        PendingTerminalAdvanceOutcome::Conflict => {
            return Ok(stale_commit(
                claimed_run.controller_generation,
                claimed_run.wake_epoch,
            ));
        }
        PendingTerminalAdvanceOutcome::NotFound => {
            return Err(moa_execution::Error::InvalidRepositoryData {
                message: "execution run disappeared during pending-terminal settlement".to_string(),
            });
        }
    };
    let activation_steps = pending_terminal_step_count(
        commit.settled_task_count,
        commit.drained_trigger_count,
        commit.cancellation_dispatches.len(),
        commit.compensation_admission.is_some(),
    )?;
    if matches!(
        commit.stage,
        PendingTerminalAdvanceStage::Finalized | PendingTerminalAdvanceStage::ManualRepairRequired
    ) {
        let mut terminal = terminal_commit(repository, scope, &commit.run).await?;
        terminal.response.activation_steps = activation_steps;
        return Ok(terminal);
    }
    if commit.compensation_admission.is_some() && commit.continuation.is_some() {
        return Err(moa_execution::Error::InvalidRepositoryData {
            message: "admitted compensation must park until its exact attempt settles".to_string(),
        });
    }
    let continuation_enqueued = commit.continuation.is_some();
    Ok(ControllerAdvanceCommit {
        response: ExecutionRunAdvanceResponse {
            outcome: response_outcome,
            controller_generation: commit.run.controller_generation,
            wake_epoch: commit.run.wake_epoch,
            activation_steps,
            materialized_tasks: 0,
            continuation_enqueued,
        },
        publish_progress: true,
        terminal_delivery: None,
    })
}

async fn terminal_commit(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run: &moa_execution::repository::ExecutionRunRecord,
) -> moa_execution::Result<ControllerAdvanceCommit> {
    let terminal_delivery = repository
        .load_bounded_terminal_delivery(scope, run.run_uid)
        .await?
        .ok_or_else(|| moa_execution::Error::InvalidRepositoryData {
            message: "terminal execution run has no bounded Session delivery".to_string(),
        })?;
    Ok(ControllerAdvanceCommit {
        response: ExecutionRunAdvanceResponse {
            outcome: ExecutionRunAdvanceOutcome::Terminal,
            controller_generation: run.controller_generation,
            wake_epoch: run.wake_epoch,
            activation_steps: 0,
            materialized_tasks: 0,
            continuation_enqueued: false,
        },
        publish_progress: true,
        terminal_delivery: Some(terminal_delivery),
    })
}

fn noop_commit(
    outcome: ExecutionRunAdvanceOutcome,
    run: &moa_execution::repository::ExecutionRunRecord,
) -> ControllerAdvanceCommit {
    ControllerAdvanceCommit {
        response: ExecutionRunAdvanceResponse {
            outcome,
            controller_generation: run.controller_generation,
            wake_epoch: run.wake_epoch,
            activation_steps: 0,
            materialized_tasks: 0,
            continuation_enqueued: false,
        },
        publish_progress: false,
        terminal_delivery: None,
    }
}

fn stale_commit(controller_generation: u64, wake_epoch: u64) -> ControllerAdvanceCommit {
    ControllerAdvanceCommit {
        response: ExecutionRunAdvanceResponse {
            outcome: ExecutionRunAdvanceOutcome::Stale,
            controller_generation,
            wake_epoch,
            activation_steps: 0,
            materialized_tasks: 0,
            continuation_enqueued: false,
        },
        publish_progress: false,
        terminal_delivery: None,
    }
}

#[cfg(test)]
pub(super) fn stale_commit_for_test(
    controller_generation: u64,
    wake_epoch: u64,
) -> ControllerAdvanceCommit {
    stale_commit(controller_generation, wake_epoch)
}

#[cfg(test)]
pub(super) fn consume_limits_for_test(
    maximum_steps: usize,
    dispatch_batch_size: usize,
    node_counts: &[usize],
    task_counts: &[usize],
) -> moa_execution::Result<(usize, usize, usize, usize)> {
    let mut limits = ActivationLimits::new(maximum_steps, dispatch_batch_size)?;
    let mut inspected = 0usize;
    let mut tasks = 0usize;
    for count in node_counts {
        inspected += limits.inspect_nodes(*count);
    }
    for count in task_counts {
        let accepted = limits.remaining_tasks.min(*count);
        limits.record_tasks(accepted)?;
        tasks += accepted;
    }
    Ok((
        inspected,
        tasks,
        limits.remaining_steps,
        limits.remaining_tasks,
    ))
}

#[cfg(test)]
pub(super) fn completion_scan_steps_for_test(
    scanned_tasks: u32,
    scanned_nodes: u32,
) -> moa_execution::Result<usize> {
    completion_scan_steps(scanned_tasks, scanned_nodes)
}

#[cfg(test)]
pub(super) fn validate_resumed_recovery_for_test(
    prior_wake_epoch: u64,
    current_wake_epoch: u64,
    continuation_enqueued: bool,
) -> moa_execution::Result<()> {
    validate_resumed_recovery_commit(prior_wake_epoch, current_wake_epoch, continuation_enqueued)
}

#[cfg(test)]
pub(super) fn validate_trigger_drain_for_test(
    prior_wake_epoch: u64,
    current_wake_epoch: u64,
    drained_trigger_count: u32,
) -> moa_execution::Result<()> {
    validate_trigger_drain_continuation(prior_wake_epoch, current_wake_epoch, drained_trigger_count)
}

#[cfg(test)]
pub(super) fn terminal_trigger_page_limit_for_test(
    remaining_steps: usize,
) -> moa_execution::Result<Option<u32>> {
    terminal_trigger_page_limit(remaining_steps)
}

#[cfg(test)]
pub(super) fn pending_terminal_step_count_for_test(
    settled_task_count: u64,
    drained_trigger_count: u32,
    cancellation_dispatch_count: usize,
    compensation_admitted: bool,
) -> moa_execution::Result<usize> {
    pending_terminal_step_count(
        settled_task_count,
        drained_trigger_count,
        cancellation_dispatch_count,
        compensation_admitted,
    )
}

#[cfg(test)]
pub(super) fn map_aggregate_requires_continuation_for_test(
    outcome: &MapAggregatePageOutcome,
) -> moa_execution::Result<bool> {
    map_aggregate_requires_continuation(outcome)
}

#[cfg(test)]
pub(super) fn validate_replan_stop_continuation_for_test(
    prior_wake_epoch: u64,
    continuation_wake_epoch: u64,
    exact_owner: bool,
) -> moa_execution::Result<()> {
    validate_replan_stop_continuation_fields(prior_wake_epoch, continuation_wake_epoch, exact_owner)
}

#[cfg(test)]
pub(super) fn activation_preflight_for_test(
    has_pending_terminal: bool,
    deadline_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> &'static str {
    match activation_preflight(has_pending_terminal, deadline_at, now) {
        ActivationPreflight::PendingTerminal => "pending_terminal",
        ActivationPreflight::DueDeadline => "due_deadline",
        ActivationPreflight::Ordinary => "ordinary",
    }
}
