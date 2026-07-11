//! Root-turn governed tool selection, dispatch, persistence, and approval routing.

use moa_core::wire::turn::TurnPhase;
use moa_core::{
    types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::identifiers::SessionId, types::session::SessionMeta,
    types::tools::TrustedSandboxFileManifestRef,
};
use restate_sdk::prelude::*;

use crate::tool_invocation::governed::{
    GovernedInvocationOrigin, GovernedInvocationOutcome, GovernedInvocationRequest,
    invoke_governed_tool, record_segment_tool_use as record_governed_segment_tool_use,
};
use crate::turn::util::{TurnEvidence, stable_tool_call_id};
use crate::turn_driver::progress as driver_progress;
use crate::workflows::turn_responsiveness::{
    ToolBudgetDecision, ToolBudgetExhausted, ToolBudgetState,
};

use super::TurnExecutionImpl;
use super::delegation::{DelegationToolRequest, handle_delegation_tool};

#[derive(Clone, Debug)]
pub(super) enum ToolDispatchOutcome {
    Completed,
    Cancelled,
    ToolBudgetExceeded(ToolBudgetExhausted),
}

pub(super) struct RootToolContext<'a> {
    pub(super) meta: &'a SessionMeta,
    pub(super) session_id: SessionId,
    pub(super) active_canary: Option<&'a str>,
    pub(super) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    pub(super) selected_procedure_skills: &'a std::collections::BTreeSet<String>,
    pub(super) turn_evidence: &'a mut TurnEvidence,
}

pub(super) async fn dispatch_response_tool_calls(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    mut tool_context: RootToolContext<'_>,
    allowed_tools: &std::collections::BTreeSet<String>,
    tool_budget: &mut ToolBudgetState,
    tool_calls: &[&ToolCallContent],
    last_summary: &mut Option<String>,
) -> Result<ToolDispatchOutcome, HandlerError> {
    for (index, tool_call) in tool_calls.iter().enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            *last_summary = Some(reason);
            return Ok(ToolDispatchOutcome::Cancelled);
        }
        if let Some(exhaustion) =
            record_tool_budget(ctx, tool_budget, &tool_call.invocation).await?
        {
            return Ok(ToolDispatchOutcome::ToolBudgetExceeded(exhaustion));
        }
        handle_tool_call(
            workflow,
            ctx,
            &mut tool_context,
            allowed_tools,
            index,
            tool_call,
        )
        .await?;
    }
    Ok(ToolDispatchOutcome::Completed)
}

pub(super) async fn record_tool_budget(
    ctx: &WorkflowContext<'_>,
    tool_budget: &mut ToolBudgetState,
    invocation: &ToolInvocation,
) -> Result<Option<ToolBudgetExhausted>, HandlerError> {
    match tool_budget.before_tool_dispatch(invocation) {
        ToolBudgetDecision::Allow {
            attempted_tool_calls,
        } => {
            driver_progress::set_tool_calls(ctx, attempted_tool_calls);
            Ok(None)
        }
        ToolBudgetDecision::Stop(exhaustion) => {
            driver_progress::set_tool_calls(ctx, tool_budget.attempted_tool_calls());
            Ok(Some(exhaustion))
        }
    }
}

async fn handle_tool_call(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tool_context: &mut RootToolContext<'_>,
    allowed_tools: &std::collections::BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let active_canary = tool_context.active_canary;
    let turn_evidence = &mut *tool_context.turn_evidence;
    let tool_id = stable_tool_call_id(session_id, index, tool_call);
    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: meta,
            session_id,
            tool_id,
            tool_call,
            allowed_tools,
            selected_procedure_skills: tool_context.selected_procedure_skills,
            active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::RootTurn,
        },
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;

    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            turn_evidence.record_tool_result(&result.invocation, &result.output);
            if result.should_record_segment_tool_use() {
                record_governed_segment_tool_use(ctx, session_id, &result.invocation.name).await?;
            }
        }
        GovernedInvocationOutcome::Delegation { tool_id, .. } => {
            handle_delegation_tool(
                workflow,
                ctx,
                DelegationToolRequest {
                    meta,
                    session_id,
                    tool_id,
                    tool_call,
                    trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
                },
                turn_evidence,
            )
            .await?;
        }
    }
    Ok(())
}
