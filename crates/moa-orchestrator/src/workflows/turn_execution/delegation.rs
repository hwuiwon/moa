//! Root coordinator worker delegation, deterministic auto-spawn, and result fan-in.

use std::time::{Duration, Instant};

use moa_brain::pipeline::delegation_planning::{
    DELEGATION_PLAN_METADATA_KEY, DelegationPlan, DelegationPlanNode, plan_delegation_for_request,
};
use moa_core::wire::turn::TurnPhase;
use moa_core::{
    types::completion::CompletionRequest, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::identifiers::SessionId,
    types::identifiers::ToolCallId, types::procedure_tools::is_procedure_tool_name,
    types::session::SessionMeta, types::tools::ToolOutput,
    types::tools::TrustedSandboxFileManifestRef,
    types::worker::commands::AttachWorkerResultWaiterInput,
    types::worker::commands::MarkWorkerChildTerminalInput,
    types::worker::commands::RemoveWorkerResultWaiterInput,
    types::worker::commands::SpawnWorkerInput, types::worker::commands::SpawnWorkerOutput,
    types::worker::state::WorkerChildRef, types::worker::state::WorkerTerminalResult,
    types::worker::state::default_worker_budget_tokens, types::worker::tool_schema::DelegationTool,
    types::worker::tool_schema::DelegationToolKind,
    types::worker::tool_schema::is_child_report_tool_name,
    types::worker::tool_schema::is_delegation_tool_name,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use moa_observability::record_turn_tool_dispatch_duration;
use moa_observability::restate_observability::tool_dispatch_span;

use crate::objects::session::{
    AutoDelegationFanInStatus, PollAutoDelegationFanInInput, RegisterAutoDelegationRunInput,
    SessionClient,
};
use crate::objects::worker::WorkerClient;
use crate::turn::util::{TurnEvidence, stable_tool_call_id};
use crate::turn_driver::progress as driver_progress;
use crate::worker_dispatch::MAX_WORKER_FAN_OUT;
use crate::workflows::errors::moa_error_to_handler_error;
use crate::workflows::turn_events::{
    append_tool_call_event, append_tool_result_event, record_segment_tool_use,
};
use crate::workflows::turn_progress::{self, SUMMARY_CHECKING_RESULTS};
use crate::workflows::turn_responsiveness::{ToolBudgetExhausted, ToolBudgetState};

use super::TurnExecutionImpl;
use super::tools::record_tool_budget;

/// Consecutive fan-in wait cycles (each up to `MAX_WAIT_TIMEOUT_MS`) on the same still-pending
/// worker before it is failed out to unblock synthesis. At 4 cycles this bounds a stuck worker
/// to roughly two minutes of silence — far beyond normal completion — while staying well under
/// the session/turn timeout.
const MAX_FAN_IN_STUCK_CYCLES: u32 = 4;

const AUTO_DELEGATION_TOOL_INDEX_BASE: usize = 10_000;
const AUTO_DELEGATION_WORKER_MAX_TURNS: u32 = 3;
const AUTO_DELEGATION_ROOT_BASE_TURNS: u32 = 4;
const AUTO_DELEGATION_ROOT_TURNS_PER_READY_NODE: u32 = 2;

pub(super) enum AutoDelegationOutcome {
    Skipped,
    Scheduled,
    Cancelled,
    ToolBudgetExceeded(ToolBudgetExhausted),
}

pub(super) enum AutoDelegationFanInOutcome {
    Skipped,
    Continue,
    Cancelled,
}

pub(super) struct AutoDelegationContext<'a> {
    pub(super) meta: &'a SessionMeta,
    pub(super) session_id: SessionId,
    pub(super) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    pub(super) turn_evidence: &'a mut TurnEvidence,
}

pub(super) async fn maybe_schedule_auto_delegation(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    mut schedule_context: AutoDelegationContext<'_>,
    request: &CompletionRequest,
    allowed_tools: &std::collections::BTreeSet<String>,
    tool_budget: &mut ToolBudgetState,
    last_summary: &mut Option<String>,
) -> Result<AutoDelegationOutcome, HandlerError> {
    if !allowed_tools.contains(DelegationToolKind::Spawn.name()) {
        return Ok(AutoDelegationOutcome::Skipped);
    }

    let Some(user_sequence_num) = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .filter(|sequence_num| *sequence_num > 0)
    else {
        return Ok(AutoDelegationOutcome::Skipped);
    };
    let scheduled_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_SEQUENCE)
        .await?
        .map(Json::into_inner);
    if scheduled_sequence_num == Some(user_sequence_num) {
        return Ok(AutoDelegationOutcome::Skipped);
    }

    let Some(plan) = delegation_plan_from_metadata(&request.metadata) else {
        return Ok(AutoDelegationOutcome::Skipped);
    };
    let ready_nodes = ready_delegation_nodes(&plan);
    let worker_slots = available_auto_worker_slots(ctx, schedule_context.session_id).await?;
    let spawn_count = ready_nodes
        .len()
        .min(MAX_WORKER_FAN_OUT)
        .min(worker_slots)
        .min(tool_budget.remaining_tool_calls());
    if spawn_count == 0 {
        return Ok(AutoDelegationOutcome::Skipped);
    }

    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let tool_subset = auto_worker_tool_subset(&workflow.tool_schemas);
    let mut worker_ids = Vec::new();
    for (index, node) in ready_nodes.into_iter().take(spawn_count).enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            *last_summary = Some(reason);
            return Ok(AutoDelegationOutcome::Cancelled);
        }

        let spawn_input = auto_spawn_input(&plan, node, &tool_subset);
        let tool_call = auto_spawn_tool_call(
            user_sequence_num,
            AUTO_DELEGATION_TOOL_INDEX_BASE + index,
            node,
            &spawn_input,
        )?;
        if let Some(exhaustion) =
            record_tool_budget(ctx, tool_budget, &tool_call.invocation).await?
        {
            return Ok(AutoDelegationOutcome::ToolBudgetExceeded(exhaustion));
        }

        let worker_id = dispatch_auto_delegation_spawn(
            workflow,
            ctx,
            &mut schedule_context,
            index,
            tool_call,
            spawn_input,
        )
        .await?;
        worker_ids.push(worker_id);
    }

    register_auto_delegation_run(
        ctx,
        schedule_context.session_id,
        user_sequence_num,
        worker_ids.clone(),
    )
    .await?;
    ctx.set(
        driver_progress::RootTurnStateKey::AUTO_DELEGATION_WORKER_IDS,
        Json::from(worker_ids),
    );
    ctx.set(
        driver_progress::RootTurnStateKey::AUTO_DELEGATION_SEQUENCE,
        Json::from(user_sequence_num),
    );
    Ok(AutoDelegationOutcome::Scheduled)
}

async fn register_auto_delegation_run(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    user_sequence_num: u64,
    worker_ids: Vec<String>,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_session_vo_call();
    ctx.object_client::<SessionClient>(session_id.to_string())
        .register_auto_delegation_run(Json::from(RegisterAutoDelegationRunInput {
            user_sequence_num,
            worker_ids,
        }))
        .call()
        .await?;
    Ok(())
}

pub(super) async fn maybe_fan_in_auto_delegation_results(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    last_summary: &mut Option<String>,
) -> Result<AutoDelegationFanInOutcome, HandlerError> {
    let Some(user_sequence_num) = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .filter(|sequence_num| *sequence_num > 0)
    else {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    };
    let scheduled_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_SEQUENCE)
        .await?
        .map(Json::into_inner);
    if scheduled_sequence_num != Some(user_sequence_num) {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    }
    let bundled_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_SEQUENCE)
        .await?
        .map(Json::into_inner);
    if bundled_sequence_num == Some(user_sequence_num) {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    }

    let worker_ids = ctx
        .get::<Json<Vec<String>>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_WORKER_IDS)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    if worker_ids.is_empty() {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    }

    // Bound the fan-in wait so one never-terminal worker (stale/hung) cannot hang the whole
    // session: track consecutive cycles spent on the same still-pending worker and, once the
    // bound is exceeded, ask the Session VO to fail it out and complete the run with the
    // partial results the coordinator can still synthesize from.
    let stuck_worker = ctx
        .get::<Json<String>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_STUCK_WORKER)
        .await?
        .map(Json::into_inner);
    let stuck_count = ctx
        .get::<Json<u32>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_STUCK_COUNT)
        .await?
        .map(Json::into_inner)
        .unwrap_or(0);
    let force_complete = stuck_count >= MAX_FAN_IN_STUCK_CYCLES;

    // Fan-in readiness is computed by the Session VO from run-owned terminal snapshots, not
    // from the transient `children` registry: a fast worker that self-cleaned (or was
    // consumed by a manual `wait_worker`) can no longer strand the bundle or make the root
    // turn synthesize before its siblings finish. The handler also emits the durable bundle
    // and claims synthesis ownership for this root turn (preventing a duplicate synthesis
    // turn on completion).
    moa_core::coordination_counters::record_session_vo_call();
    let status = ctx
        .object_client::<SessionClient>(session_id.to_string())
        .poll_auto_delegation_fan_in(Json::from(PollAutoDelegationFanInInput {
            user_sequence_num,
            root_turn_id: turn_id.to_string(),
            force_complete,
        }))
        .call()
        .await?
        .into_inner();
    match status {
        AutoDelegationFanInStatus::Ready => {
            ctx.set(
                driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_SEQUENCE,
                Json::from(user_sequence_num),
            );
            Ok(AutoDelegationFanInOutcome::Continue)
        }
        AutoDelegationFanInStatus::Pending { worker_id } => {
            // Count consecutive cycles on the same worker; reset when fan-in makes progress
            // (a different worker becomes the first still-pending one).
            let next_count = if stuck_worker.as_deref() == Some(worker_id.as_str()) {
                stuck_count.saturating_add(1)
            } else {
                1
            };
            ctx.set(
                driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_STUCK_WORKER,
                Json::from(worker_id.clone()),
            );
            ctx.set(
                driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_STUCK_COUNT,
                Json::from(next_count),
            );
            wait_for_auto_delegation_worker(workflow, ctx, session_id, &worker_id, last_summary)
                .await
        }
        AutoDelegationFanInStatus::NotRunning => Ok(AutoDelegationFanInOutcome::Skipped),
    }
}

async fn wait_for_auto_delegation_worker(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    worker_id: &str,
    last_summary: &mut Option<String>,
) -> Result<AutoDelegationFanInOutcome, HandlerError> {
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(AutoDelegationFanInOutcome::Cancelled);
    }

    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        SUMMARY_CHECKING_RESULTS,
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;

    let (awakeable_id, terminal_future) = ctx.awakeable::<String>();
    moa_core::coordination_counters::record_worker_vo_call();
    let attached = ctx
        .object_client::<WorkerClient>(worker_id.to_string())
        .attach_result_waiter(Json::from(AttachWorkerResultWaiterInput {
            awakeable_id: awakeable_id.clone(),
        }))
        .call()
        .await?
        .into_inner();
    if let Some(terminal) = attached.terminal {
        cache_auto_delegation_terminal(ctx, session_id, worker_id, terminal).await?;
        return Ok(AutoDelegationFanInOutcome::Continue);
    }

    restate_sdk::select! {
        reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
            remove_auto_delegation_result_waiter(ctx, worker_id, awakeable_id).await?;
            let reason = reason?;
            *last_summary = Some(reason);
            Ok(AutoDelegationFanInOutcome::Cancelled)
        },
        terminal = terminal_future => {
            let terminal = terminal?;
            let terminal = serde_json::from_str::<WorkerTerminalResult>(&terminal).map_err(|error| {
                TerminalError::new(format!(
                    "failed to decode auto delegation terminal result: {error}"
                ))
            })?;
            cache_auto_delegation_terminal(ctx, session_id, worker_id, terminal).await?;
            Ok(AutoDelegationFanInOutcome::Continue)
        },
        _ = ctx.sleep(Duration::from_millis(crate::delegation::MAX_WAIT_TIMEOUT_MS)) => {
            remove_auto_delegation_result_waiter(ctx, worker_id, awakeable_id).await?;
            Ok(AutoDelegationFanInOutcome::Continue)
        }
    }
}

async fn cache_auto_delegation_terminal(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    worker_id: &str,
    terminal: WorkerTerminalResult,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_session_vo_call();
    ctx.object_client::<SessionClient>(session_id.to_string())
        .mark_child_terminal(Json::from(MarkWorkerChildTerminalInput {
            worker_id: worker_id.to_string(),
            terminal,
        }))
        .call()
        .await?;
    Ok(())
}

async fn remove_auto_delegation_result_waiter(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
    awakeable_id: String,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(worker_id.to_string())
        .remove_result_waiter(Json::from(RemoveWorkerResultWaiterInput { awakeable_id }))
        .call()
        .await?;
    Ok(())
}

fn delegation_plan_from_metadata(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<DelegationPlan> {
    metadata
        .get(DELEGATION_PLAN_METADATA_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn ready_delegation_nodes(plan: &DelegationPlan) -> Vec<&DelegationPlanNode> {
    plan.nodes
        .iter()
        .filter(|node| node.depends_on.is_empty())
        .collect()
}

pub(super) fn root_request_turn_cap_for_auto_delegation(
    user_message: &str,
    request_max_turns: Option<u32>,
) -> Option<u32> {
    let Some(delegation_cap) = auto_delegation_root_turn_cap(user_message) else {
        return request_max_turns;
    };
    Some(request_max_turns.map_or(delegation_cap, |cap| cap.max(delegation_cap)))
}

fn auto_delegation_root_turn_cap(user_message: &str) -> Option<u32> {
    let plan = plan_delegation_for_request(user_message)?;
    let ready_node_count = ready_delegation_nodes(&plan).len().min(MAX_WORKER_FAN_OUT);
    if ready_node_count == 0 {
        return None;
    }
    let ready_node_count = u32::try_from(ready_node_count).unwrap_or(u32::MAX);
    Some(
        AUTO_DELEGATION_ROOT_BASE_TURNS.saturating_add(
            ready_node_count.saturating_mul(AUTO_DELEGATION_ROOT_TURNS_PER_READY_NODE),
        ),
    )
}

/// Builds the tool subset granted to auto-delegated workers.
///
/// Workers receive the full configured execution surface — derived from the
/// unfiltered precompiled tool schemas, not the coordinator's sandbox-free
/// allowlist — so delegated compute keeps hand-routed tools like `bash` that
/// the root coordinator itself can never call. Coordinator-side delegation
/// controls, child-report tools, and workflow-owned procedure tools stay
/// excluded. Once delegation plan nodes carry per-task tool requirements,
/// derive per-node subsets from the plan instead of granting the full surface.
fn auto_worker_tool_subset(schemas: &[serde_json::Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|schema| schema.get("name").and_then(serde_json::Value::as_str))
        .filter(|name| {
            !is_delegation_tool_name(name)
                && !is_child_report_tool_name(name)
                && !is_procedure_tool_name(name)
        })
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn available_auto_worker_slots(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<usize, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    Ok(remaining_worker_capacity(&children))
}

async fn session_child_refs(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<WorkerChildRef>, HandlerError> {
    moa_core::coordination_counters::record_session_vo_call();
    Ok(ctx
        .object_client::<SessionClient>(session_id.to_string())
        .child_refs()
        .call()
        .await?
        .into_inner())
}

fn remaining_worker_capacity(children: &[WorkerChildRef]) -> usize {
    let active_children = children
        .iter()
        .filter(|child| child.terminal.is_none())
        .count();
    MAX_WORKER_FAN_OUT.saturating_sub(active_children)
}

fn auto_spawn_input(
    _plan: &DelegationPlan,
    node: &DelegationPlanNode,
    tool_subset: &[String],
) -> SpawnWorkerInput {
    SpawnWorkerInput {
        task: format!(
            concat!(
                "Subtask: {}\n\n",
                "Return a concise result for the coordinator to synthesize. Include:\n",
                "- what you found or did;\n",
                "- evidence, source ids, or tool outputs you relied on;\n",
                "- open questions, blockers, or missing input;\n",
                "- recommended next step if the subtask is not complete.\n\n",
                "If required facts or inputs are missing, report the blocker or missing input ",
                "instead of inventing facts.\n\n",
                "Do not answer the user directly. Report back to the coordinator."
            ),
            node.title
        ),
        tool_subset: tool_subset.to_vec(),
        budget_tokens: default_worker_budget_tokens(),
        max_turns: Some(AUTO_DELEGATION_WORKER_MAX_TURNS),
    }
}

fn auto_spawn_tool_call(
    user_sequence_num: u64,
    stable_index: usize,
    node: &DelegationPlanNode,
    spawn_input: &SpawnWorkerInput,
) -> Result<ToolCallContent, HandlerError> {
    Ok(ToolCallContent {
        invocation: ToolInvocation {
            id: Some(auto_delegation_provider_tool_id(
                user_sequence_num,
                stable_index,
                &node.id,
            )),
            name: DelegationToolKind::Spawn.name().to_string(),
            input: serde_json::to_value(spawn_input).map_err(|error| {
                TerminalError::new(format!(
                    "failed to serialize auto delegation input: {error}"
                ))
            })?,
        },
        provider_metadata: None,
    })
}

fn auto_delegation_provider_tool_id(
    user_sequence_num: u64,
    stable_index: usize,
    node_id: &str,
) -> String {
    format!(
        "fc_auto_delegation_{user_sequence_num}_{stable_index}_{}",
        provider_safe_id_segment(node_id)
    )
}

fn provider_safe_id_segment(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches('_');
    if safe.is_empty() {
        "node".to_string()
    } else {
        safe.to_string()
    }
}

async fn dispatch_auto_delegation_spawn(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    schedule_context: &mut AutoDelegationContext<'_>,
    index: usize,
    tool_call: ToolCallContent,
    spawn_input: SpawnWorkerInput,
) -> Result<String, HandlerError> {
    let invocation = tool_call.invocation.clone();
    let tool_id = stable_tool_call_id(
        schedule_context.session_id,
        AUTO_DELEGATION_TOOL_INDEX_BASE + index,
        &tool_call,
    );
    append_tool_call_event(ctx, schedule_context.session_id, tool_id, &tool_call).await?;

    turn_progress::maybe_emit(
        ctx,
        schedule_context.session_id,
        turn_progress::running_tool_summary(&invocation.name),
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::RootSession {
            session_id: schedule_context.session_id,
            meta: schedule_context.meta,
        },
        DelegationTool::Spawn(spawn_input),
        schedule_context.trusted_sandbox_manifest,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);
    let worker_id = spawn_worker_id_from_output(&output)?;

    append_auto_delegation_result(
        ctx,
        schedule_context.session_id,
        tool_id,
        &invocation,
        output,
        schedule_context.turn_evidence,
    )
    .await?;
    Ok(worker_id)
}

fn spawn_worker_id_from_output(output: &ToolOutput) -> Result<String, HandlerError> {
    let structured = output
        .structured
        .clone()
        .ok_or_else(|| TerminalError::new("spawn_worker returned no structured output"))?;
    let output = serde_json::from_value::<SpawnWorkerOutput>(structured).map_err(|error| {
        TerminalError::new(format!(
            "failed to decode auto delegation spawn output: {error}"
        ))
    })?;
    Ok(output.worker_id)
}

async fn append_auto_delegation_result(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: ToolOutput,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    append_tool_result_event(ctx, session_id, tool_id, invocation, &output).await?;
    turn_evidence.record_tool_result(invocation, &output);

    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

pub(super) struct DelegationToolRequest<'a> {
    pub(super) meta: &'a SessionMeta,
    pub(super) session_id: SessionId,
    pub(super) tool_id: ToolCallId,
    pub(super) tool_call: &'a ToolCallContent,
    pub(super) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
}

pub(super) async fn handle_delegation_tool(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: DelegationToolRequest<'_>,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    let DelegationToolRequest {
        meta,
        session_id,
        tool_id,
        tool_call,
        trusted_sandbox_manifest,
    } = request;
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
    let Some(tool) =
        moa_core::types::worker::tool_schema::DelegationTool::from_invocation(&invocation)
            .map_err(moa_error_to_handler_error)?
    else {
        return Err(
            TerminalError::new(format!("unsupported delegation tool {}", invocation.name)).into(),
        );
    };

    let span = tool_dispatch_span(&invocation.name);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_progress::running_tool_summary(&invocation.name),
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::RootSession { session_id, meta },
        tool,
        trusted_sandbox_manifest,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
    turn_evidence.record_tool_result(&invocation, &output);

    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn auto_delegation_turn_cap_raises_low_explicit_root_cap() {
        // Pins: multi-worker fan-out leaves enough coordinator turns for wait and synthesis.
        let request =
            "Plan an A/B test readout using activation, retention, and support-ticket signals.";

        assert_eq!(
            root_request_turn_cap_for_auto_delegation(request, Some(6)),
            Some(10)
        );
    }

    #[test]
    fn auto_delegation_turn_cap_keeps_higher_explicit_root_cap() {
        // Pins: caller-provided headroom is not reduced by deterministic delegation planning.
        let request =
            "Plan an A/B test readout using activation, retention, and support-ticket signals.";

        assert_eq!(
            root_request_turn_cap_for_auto_delegation(request, Some(14)),
            Some(14)
        );
    }

    #[test]
    fn auto_delegation_turn_cap_leaves_non_delegable_turns_unchanged() {
        // Pins: direct asks keep their original responsiveness cap.
        assert_eq!(
            root_request_turn_cap_for_auto_delegation("What is the status?", Some(6)),
            Some(6)
        );
        assert_eq!(
            root_request_turn_cap_for_auto_delegation("What is the status?", None),
            None
        );
    }

    #[test]
    fn auto_delegation_uses_only_ready_dag_nodes() {
        // Pins: deterministic scheduling can parallelize ready work without crossing dependencies.
        let plan = DelegationPlan {
            reason: "explicit_multi_workstream_list".to_string(),
            nodes: vec![
                DelegationPlanNode {
                    id: "node-1".to_string(),
                    title: "support tickets".to_string(),
                    depends_on: Vec::new(),
                },
                DelegationPlanNode {
                    id: "node-2".to_string(),
                    title: "billing logs".to_string(),
                    depends_on: Vec::new(),
                },
                DelegationPlanNode {
                    id: "node-3".to_string(),
                    title: "final synthesis".to_string(),
                    depends_on: vec!["node-1".to_string(), "node-2".to_string()],
                },
            ],
        };

        let ready = ready_delegation_nodes(&plan)
            .into_iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ready, vec!["node-1", "node-2"]);
    }

    mod auto_delegation {
        use super::*;

        #[test]
        fn auto_worker_tool_subset_preserves_current_management_tool_exclusions() {
            // Pins: auto-spawned workers inherit the full configured execution
            // surface — including sandbox tools like bash/file_write that the
            // sandbox-free coordinator can never call — while delegation controls,
            // child-report tools, and procedure tools stay excluded.
            let schemas = vec![
                serde_json::json!({"name": "bash"}),
                serde_json::json!({"name": "cancel_worker"}),
                serde_json::json!({"name": "file_read"}),
                serde_json::json!({"name": "file_write"}),
                serde_json::json!({"name": "list_workers"}),
                serde_json::json!({"name": "message_worker"}),
                serde_json::json!({"name": "procedure_status"}),
                serde_json::json!({"name": "provide_worker_input"}),
                serde_json::json!({"name": "request_input"}),
                serde_json::json!({"name": "report_to_parent"}),
                serde_json::json!({"name": "run_procedure"}),
                serde_json::json!({"name": "spawn_worker"}),
                serde_json::json!({"name": "wait_worker"}),
                serde_json::json!({"name": "web_fetch"}),
            ];

            assert_eq!(
                auto_worker_tool_subset(&schemas),
                vec![
                    "bash".to_string(),
                    "file_read".to_string(),
                    "file_write".to_string(),
                    "web_fetch".to_string()
                ]
            );
        }
    }

    #[test]
    fn auto_delegation_capacity_ignores_terminal_children() {
        // Pins: deterministic scheduling does not fail a turn when active worker slots are full.
        let active = WorkerChildRef {
            id: "active-worker".to_string(),
            task_hash: "active".to_string(),
            budget_tokens: 128,
            terminal: None,
        };
        let terminal = WorkerChildRef {
            id: "done-worker".to_string(),
            task_hash: "done".to_string(),
            budget_tokens: 128,
            terminal: Some(moa_core::types::worker::state::WorkerTerminalResult {
                state: moa_core::types::worker::state::WorkerState::Completed,
                result: moa_core::types::worker::state::WorkerResult {
                    worker_id: "done-worker".to_string(),
                    success: true,
                    output: "done".to_string(),
                    tokens_used: 32,
                    tools_invoked: 0,
                    error: None,
                },
            }),
        };

        let mut children = vec![active; MAX_WORKER_FAN_OUT];
        assert_eq!(remaining_worker_capacity(&children), 0);

        children.pop();
        children.push(terminal);
        assert_eq!(remaining_worker_capacity(&children), 1);
    }

    // Fan-in readiness/ordering is now owned by `SessionVoState` (run-owned terminal
    // snapshots); those behaviors are pinned by unit tests in `objects::session::state`
    // (`auto_delegation_bundle_*`), which also cover the self-cleanup / consume races that
    // the former child-registry readiness helper could not.

    #[test]
    fn auto_delegation_spawn_input_uses_general_purpose_task_envelope() {
        // Pins: scheduling keeps `spawn_worker.task` as the generic envelope and applies child caps.
        let plan = DelegationPlan {
            reason: "explicit_comparison".to_string(),
            nodes: Vec::new(),
        };
        let node = DelegationPlanNode {
            id: "node-1".to_string(),
            title: "finance assumptions".to_string(),
            depends_on: Vec::new(),
        };

        let input = auto_spawn_input(&plan, &node, &["file_read".to_string()]);

        assert_eq!(input.tool_subset, vec!["file_read".to_string()]);
        assert_eq!(input.budget_tokens, default_worker_budget_tokens());
        assert_eq!(input.max_turns, Some(AUTO_DELEGATION_WORKER_MAX_TURNS));
        assert_eq!(
            input.task,
            "Subtask: finance assumptions\n\n\
             Return a concise result for the coordinator to synthesize. Include:\n\
             - what you found or did;\n\
             - evidence, source ids, or tool outputs you relied on;\n\
             - open questions, blockers, or missing input;\n\
             - recommended next step if the subtask is not complete.\n\n\
             If required facts or inputs are missing, report the blocker or missing input \
             instead of inventing facts.\n\n\
             Do not answer the user directly. Report back to the coordinator."
        );
    }

    #[test]
    fn auto_delegation_spawn_input_has_no_task_name() {
        // Pins: serialized deterministic auto-spawns keep the smaller worker contract.
        let plan = DelegationPlan {
            reason: "explicit_comparison".to_string(),
            nodes: Vec::new(),
        };
        let node = DelegationPlanNode {
            id: "node-1".to_string(),
            title: "finance assumptions".to_string(),
            depends_on: Vec::new(),
        };
        let input = auto_spawn_input(&plan, &node, &["file_read".to_string()]);

        let tool_call = auto_spawn_tool_call(42, 10_000, &node, &input)
            .expect("spawn tool call should serialize");
        let payload = tool_call
            .invocation
            .input
            .as_object()
            .expect("auto-spawn input should serialize as a JSON object");
        let keys = payload
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let expected_keys = ["budget_tokens", "max_turns", "task", "tool_subset"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(keys, expected_keys);
        assert!(
            payload.get("task_name").is_none(),
            "auto-spawn input should not serialize removed task_name"
        );
    }

    #[test]
    fn auto_delegation_tool_call_looks_like_spawn_worker() {
        // Pins: deterministic auto-spawns are represented as ordinary spawn_worker tool calls.
        let input = SpawnWorkerInput {
            task: "Review support tickets.".to_string(),
            tool_subset: vec!["file_read".to_string()],
            budget_tokens: 512,
            max_turns: Some(2),
        };
        let node = DelegationPlanNode {
            id: "node-1".to_string(),
            title: "support tickets".to_string(),
            depends_on: Vec::new(),
        };

        let tool_call = auto_spawn_tool_call(42, 10_000, &node, &input)
            .expect("spawn tool call should serialize");

        assert_eq!(tool_call.invocation.name, "spawn_worker");
        assert_eq!(
            tool_call.invocation.id.as_deref(),
            Some("fc_auto_delegation_42_10000_node_1")
        );
        let provider_id = tool_call
            .invocation
            .id
            .as_ref()
            .expect("auto-delegation tool call should have provider id");
        assert!(provider_id.starts_with("fc_"));
        assert!(
            provider_id
                .chars()
                .all(|ch| { ch.is_ascii_alphanumeric() || ch == '_' })
        );
        assert_eq!(
            tool_call.invocation.input["task"],
            json!("Review support tickets.")
        );
        assert!(
            tool_call.invocation.input.get("task_name").is_none(),
            "auto-spawn input should not serialize removed task_name"
        );
    }

    #[test]
    fn auto_delegation_tool_call_sanitizes_node_id_for_provider_replay() {
        // Pins: synthetic tool calls must be replayable through providers that only accept
        // letters, numbers, underscores, or dashes in call ids.
        let input = SpawnWorkerInput {
            task: "Review support tickets.".to_string(),
            tool_subset: vec!["file_read".to_string()],
            budget_tokens: 512,
            max_turns: Some(2),
        };
        let node = DelegationPlanNode {
            id: "finance:model/v1".to_string(),
            title: "support tickets".to_string(),
            depends_on: Vec::new(),
        };

        let tool_call = auto_spawn_tool_call(42, 10_000, &node, &input)
            .expect("spawn tool call should serialize");

        assert_eq!(
            tool_call.invocation.id.as_deref(),
            Some("fc_auto_delegation_42_10000_finance_model_v1")
        );
    }
}
