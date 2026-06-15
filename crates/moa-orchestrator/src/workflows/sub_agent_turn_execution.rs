//! Workflow-backed execution for one sub-agent turn run.
//!
//! The `SubAgent` virtual object owns conversational state and message
//! admission. This workflow owns the repeated LLM/tool loop so `post_message`
//! can return quickly and child execution has a durable progress/cancellation
//! surface like top-level session turns.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_core::restate_observability::{
    annotate_restate_handler_span, event_persist_span, llm_call_span, tool_dispatch_span,
};
use moa_core::wire::{
    AppendEventRequest, RecordSegmentToolUseRequest, RunSubAgentTurnRequest, TurnOutcome,
    TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{
    ApprovalDecision, ApprovalPrompt, ClearSubAgentPendingApprovalInput, CompletionRequest, Event,
    PolicyAction, SessionId, SessionMeta, SetSubAgentPendingApprovalInput, SubAgentToolRecord,
    SubAgentTurnOutcomeRecord, SubAgentTurnPreparation, SubAgentTurnResponseRecord,
    ToolCallContent, ToolCallId, ToolCallRequest, ToolInvocation, ToolOutput,
    TurnOutcome as CoreTurnOutcome, is_delegation_tool_name, record_approval_wait,
    record_turn_event_persist_duration, record_turn_llm_call_duration,
    record_turn_tool_dispatch_duration,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::objects::sub_agent::{MAX_SUB_AGENT_TURNS_PER_WORKFLOW, SubAgentClient};
use crate::services::{
    llm_gateway::LLMGatewayClient,
    session_store::RestateSessionStoreClient,
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareToolApprovalRequest, StoreApprovalRuleRequest, WorkspaceStoreClient},
};
use crate::turn::approval::{parse_awakeable_decision, serialize_awakeable_decision};
use crate::turn::util::{
    allowed_tool_names, denied_tool_output, disallowed_tool_output, meaningful_cancel_reason,
    response_tool_calls, stable_tool_call_id, tool_call_is_allowed, turn_outcome_for_response,
};
use crate::workflows::approval_wait;

const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_PHASE: &str = "phase";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PendingApprovalState {
    awakeable_id: String,
    request_id: uuid::Uuid,
    session_id: SessionId,
    sub_agent_id: String,
}

#[derive(Clone, Debug)]
struct ApprovalOutcome {
    allow_execution: bool,
    denied_output: ToolOutput,
}

impl ApprovalOutcome {
    fn allow_execution() -> Self {
        Self {
            allow_execution: true,
            denied_output: ToolOutput::error("", Duration::ZERO),
        }
    }

    fn deny(denied_output: ToolOutput) -> Self {
        Self {
            allow_execution: false,
            denied_output,
        }
    }
}

/// Restate workflow surface for durable sub-agent turn execution.
#[restate_sdk::workflow]
pub trait SubAgentTurnExecution {
    /// Runs one sub-agent turn workflow body.
    async fn run(request: Json<RunSubAgentTurnRequest>) -> Result<Json<TurnOutcome>, HandlerError>;

    /// Requests cancellation of the in-flight sub-agent turn workflow.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;

    /// Returns workflow progress without blocking the workflow body.
    #[shared]
    async fn progress() -> Result<Json<TurnProgress>, HandlerError>;
}

/// Concrete `SubAgentTurnExecution` workflow implementation.
pub struct SubAgentTurnExecutionImpl;

impl SubAgentTurnExecution for SubAgentTurnExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunSubAgentTurnRequest>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("SubAgentTurnExecution", "run");
        let request = request.into_inner();
        ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));

        let outcome = match run_sub_agent_inside_workflow(&ctx, &request).await {
            Ok(outcome) => outcome,
            Err(error) => TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Failed,
                message: format!("{error:?}"),
            },
        };
        if matches!(outcome.kind, TurnOutcomeKind::Cancelled) {
            cleanup_pending_approval_after_cancel(
                &ctx,
                &request.turn_id,
                &request.sub_agent_id,
                &outcome.message,
            )
            .await?;
        }
        let phase = match outcome.kind {
            TurnOutcomeKind::Completed => TurnPhase::Completed,
            TurnOutcomeKind::Cancelled => TurnPhase::Cancelled,
            TurnOutcomeKind::Failed => TurnPhase::Failed,
        };
        ctx.set(K_PHASE, Json::from(phase));
        notify_sub_agent_of_outcome(&ctx, &request.sub_agent_id, &outcome);
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgentTurnExecution", "request_cancel");
        let phase = ctx
            .get::<Json<TurnPhase>>(K_PHASE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        if is_terminal_phase(&phase) {
            return Ok(());
        }

        let Some(reason) = meaningful_cancel_reason(Some(reason.into_inner())) else {
            return Ok(());
        };
        ctx.resolve_promise(K_CANCEL_REASON_PROMISE, reason);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        annotate_restate_handler_span("SubAgentTurnExecution", "progress");
        let phase = ctx
            .get::<Json<TurnPhase>>(K_PHASE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let cancel_reason =
            meaningful_cancel_reason(ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE).await?);
        Ok(Json::from(TurnProgress {
            turn_id: ctx.key().to_string(),
            phase,
            cancel_requested: cancel_reason.is_some(),
            cancel_reason,
        }))
    }
}

async fn run_sub_agent_inside_workflow(
    ctx: &WorkflowContext<'_>,
    request: &RunSubAgentTurnRequest,
) -> Result<TurnOutcome, HandlerError> {
    for _turn_number in 1..=MAX_SUB_AGENT_TURNS_PER_WORKFLOW {
        if let Some(reason) = cancel_requested(ctx).await? {
            ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
                .cancel(reason.clone())
                .send();
            return Ok(TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
            });
        }

        ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));
        let preparation = ctx
            .object_client::<SubAgentClient>(request.sub_agent_id.clone())
            .prepare_turn()
            .call()
            .await?
            .into_inner();
        let (mut completion_request, active_canary, meta, parent_session) = match preparation {
            SubAgentTurnPreparation::Outcome { outcome } => {
                return Ok(workflow_outcome_from_core(request, outcome));
            }
            SubAgentTurnPreparation::Request {
                request,
                active_canary,
                session_meta,
                parent_session,
            } => (*request, active_canary, *session_meta, parent_session),
        };
        attach_active_segment_metadata(ctx, parent_session, &mut completion_request).await?;
        let allowed_tools = allowed_tool_names(&completion_request);

        ctx.set(K_PHASE, Json::from(TurnPhase::Streaming));
        let span = llm_call_span(&meta);
        let llm_started = Instant::now();
        let response = {
            let _guard = span.enter();
            restate_sdk::select! {
                reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                    let reason = reason?;
                    ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
                        .cancel(reason.clone())
                        .send();
                    return Ok(TurnOutcome {
                        turn_id: request.turn_id.clone(),
                        kind: TurnOutcomeKind::Cancelled,
                        message: reason,
                    });
                },
                response = ctx
                    .service_client::<LLMGatewayClient>()
                    .complete(Json::from(completion_request))
                    .call() => {
                        response?.into_inner()
                    }
            }
        };
        record_turn_llm_call_duration(llm_started.elapsed());

        ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
            .record_response(Json::from(SubAgentTurnResponseRecord {
                turn_id: request.turn_id.clone(),
                response: response.clone(),
            }))
            .call()
            .await?;

        for (index, tool_call) in response_tool_calls(&response).into_iter().enumerate() {
            if let Some(reason) = cancel_requested(ctx).await? {
                ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
                    .cancel(reason.clone())
                    .send();
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message: reason,
                });
            }
            let tool_context = SubAgentToolContext {
                turn_id: &request.turn_id,
                sub_agent_id: &request.sub_agent_id,
                meta: &meta,
                session_id: parent_session,
                active_canary: active_canary.as_deref(),
            };
            handle_tool_call(ctx, tool_context, &allowed_tools, index, tool_call).await?;
        }

        let outcome = turn_outcome_for_response(&response);
        ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
            .apply_turn_outcome(Json::from(SubAgentTurnOutcomeRecord {
                turn_id: request.turn_id.clone(),
                outcome,
            }))
            .call()
            .await?;
        match outcome {
            CoreTurnOutcome::Continue => continue,
            CoreTurnOutcome::Idle => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message: "sub-agent turn completed".to_string(),
                });
            }
            CoreTurnOutcome::WaitingApproval => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message: "sub-agent turn is waiting for approval".to_string(),
                });
            }
            CoreTurnOutcome::Cancelled => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message: "sub-agent turn cancelled".to_string(),
                });
            }
        }
    }

    Ok(TurnOutcome {
        turn_id: request.turn_id.clone(),
        kind: TurnOutcomeKind::Failed,
        message: format!("sub-agent turn budget exceeded ({MAX_SUB_AGENT_TURNS_PER_WORKFLOW})"),
    })
}

async fn attach_active_segment_metadata(
    ctx: &WorkflowContext<'_>,
    parent_session: SessionId,
    request: &mut CompletionRequest,
) -> Result<(), HandlerError> {
    let Some(segment) = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(parent_session))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view())
    else {
        return Ok(());
    };
    request.metadata.insert(
        "_moa.segment_id".to_string(),
        serde_json::json!(segment.id.to_string()),
    );
    request.metadata.insert(
        "_moa.segment_index".to_string(),
        serde_json::json!(segment.segment_index),
    );
    Ok(())
}

struct SubAgentToolContext<'a> {
    turn_id: &'a str,
    sub_agent_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    active_canary: Option<&'a str>,
}

async fn handle_tool_call(
    ctx: &WorkflowContext<'_>,
    tool_context: SubAgentToolContext<'_>,
    allowed_tools: &BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    ctx.set(K_PHASE, Json::from(TurnPhase::Tooling));
    let sub_agent_id = tool_context.sub_agent_id;
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let tool_id = stable_tool_call_id(session_id, index, tool_call);
    let invocation = tool_call.invocation.clone();

    if !tool_call_is_allowed(allowed_tools, &invocation.name) {
        append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
        let output = disallowed_tool_output(&invocation.name);
        append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
        record_denied_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            tool_id,
            &invocation,
            &output,
        )
        .await?;
        return Ok(());
    }

    if invocation.name == "dispatch_sub_agent" {
        handle_dispatch(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            session_id,
            tool_id,
            tool_call,
        )
        .await?;
        return Ok(());
    }

    if is_delegation_tool_name(&invocation.name) {
        handle_delegation_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            session_id,
            tool_id,
            tool_call,
        )
        .await?;
        return Ok(());
    }

    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let policy = ctx
        .service_client::<WorkspaceStoreClient>()
        .prepare_tool_approval(Json(PrepareToolApprovalRequest {
            session: meta.clone(),
            invocation: invocation.clone(),
            request_id: tool_id.0,
        }))
        .call()
        .await?
        .into_inner();

    if matches!(policy.action, PolicyAction::Deny) {
        append_session_event(
            ctx,
            session_id,
            Event::ToolError {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                tool_name: invocation.name.clone(),
                error: format!("tool {} denied by policy", invocation.name),
                retryable: false,
            },
        )
        .await?;
        let output = denied_tool_output(format!("Tool {} denied by policy", invocation.name));
        record_denied_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            tool_id,
            &invocation,
            &output,
        )
        .await?;
        return Ok(());
    }

    if matches!(policy.action, PolicyAction::RequireApproval) {
        let decided =
            handle_approval_gate(ctx, &tool_context, &invocation, tool_id, policy.prompt).await?;
        if !decided.allow_execution {
            append_tool_result_event(
                ctx,
                session_id,
                tool_id,
                &invocation,
                &decided.denied_output,
            )
            .await?;
            record_denied_tool(
                ctx,
                tool_context.turn_id,
                sub_agent_id,
                tool_id,
                &invocation,
                &decided.denied_output,
            )
            .await?;
            return Ok(());
        }
    }

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = ctx
        .service_client::<ToolExecutorClient>()
        .execute(Json::from(ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            active_canary: tool_context.active_canary.map(ToOwned::to_owned),
            session_id: Some(session_id),
            workspace_id: meta.workspace_id.clone(),
            user_id: meta.user_id.clone(),
            idempotency_key: invocation.id.clone(),
        }))
        .call()
        .instrument(span)
        .await?
        .into_inner();
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    record_tool_result(
        ctx,
        tool_context.turn_id,
        sub_agent_id,
        tool_id,
        &invocation,
        &output,
    )
    .await?;
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn handle_dispatch(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    handle_delegation_tool(
        ctx,
        turn_id,
        parent_sub_agent_id,
        session_id,
        tool_id,
        tool_call,
    )
    .await
}

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
    let Some(tool) = moa_core::DelegationTool::from_invocation(&invocation)
        .map_err(|error| TerminalError::new(error.to_string()))?
    else {
        return Err(
            TerminalError::new(format!("unsupported delegation tool {}", invocation.name)).into(),
        );
    };

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::SubAgent {
            sub_agent_id: parent_sub_agent_id,
            session_id,
        },
        tool,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_delegation_tool_result(ctx, session_id, tool_id, &invocation, &output).await?;
    record_tool_result(
        ctx,
        turn_id,
        parent_sub_agent_id,
        tool_id,
        &invocation,
        &output,
    )
    .await?;
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

#[cfg(test)]
fn parent_session_from_initial_message(
    message: &moa_core::SubAgentMessage,
) -> Result<SessionId, HandlerError> {
    match message {
        moa_core::SubAgentMessage::InitialTask { parent_session, .. } => Ok(*parent_session),
        moa_core::SubAgentMessage::FollowUp { .. }
        | moa_core::SubAgentMessage::ChildResult { .. } => {
            Err(TerminalError::new("reserved child did not include an initial task message").into())
        }
    }
}

async fn handle_approval_gate(
    ctx: &WorkflowContext<'_>,
    tool_context: &SubAgentToolContext<'_>,
    invocation: &ToolInvocation,
    tool_id: ToolCallId,
    prompt: Option<ApprovalPrompt>,
) -> Result<ApprovalOutcome, HandlerError> {
    let turn_id = tool_context.turn_id;
    let sub_agent_id = tool_context.sub_agent_id;
    let session_id = tool_context.session_id;
    let meta = tool_context.meta;
    let mut prompt = prompt.ok_or_else(|| {
        TerminalError::new(format!(
            "workspace store did not return an approval prompt for tool {}",
            invocation.name
        ))
    })?;
    let (awakeable_id, awakeable) = ctx.awakeable::<String>();
    let pending = PendingApprovalState {
        awakeable_id: awakeable_id.clone(),
        request_id: tool_id.0,
        session_id,
        sub_agent_id: sub_agent_id.to_string(),
    };
    ctx.set(K_PENDING_APPROVAL, Json::from(pending));
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .set_pending_approval(Json::from(SetSubAgentPendingApprovalInput {
            turn_id: turn_id.to_string(),
            awakeable_id: awakeable_id.clone(),
        }))
        .call()
        .await?;

    prompt.request.sub_agent_id = Some(sub_agent_id.to_string());
    append_session_event(
        ctx,
        session_id,
        Event::ApprovalRequested {
            request_id: prompt.request.request_id,
            awakeable_id: Some(awakeable_id),
            sub_agent_id: Some(sub_agent_id.to_string()),
            tool_name: prompt.request.tool_name.clone(),
            input_summary: prompt.request.input_summary.clone(),
            risk_level: prompt.request.risk_level.clone(),
            prompt: prompt.clone(),
        },
    )
    .await?;

    let approval_timeout = approval_wait::configured_timeout();
    let timed_out_reason = approval_wait::timeout_reason(approval_timeout);
    let approval_started = Instant::now();
    let decision = restate_sdk::select! {
        decision = awakeable => {
            parse_awakeable_decision(&decision?)?
        },
        reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
            ApprovalDecision::Deny {
                reason: Some(approval_wait::cancel_reason(&reason?)),
            }
        },
        _ = ctx.sleep(approval_timeout) => {
            ApprovalDecision::Deny {
                reason: Some(timed_out_reason.clone()),
            }
        }
    };
    record_approval_wait(
        approval_started.elapsed(),
        approval_wait::outcome_label(&decision, &timed_out_reason),
    );

    ctx.clear(K_PENDING_APPROVAL);
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .clear_pending_approval(Json::from(ClearSubAgentPendingApprovalInput {
            turn_id: turn_id.to_string(),
        }))
        .call()
        .await?;

    let decided_by = approval_wait::system_decider_for(&decision, &timed_out_reason)
        .unwrap_or_else(|| meta.user_id.as_str())
        .to_string();

    append_session_event(
        ctx,
        session_id,
        Event::ApprovalDecided {
            request_id: prompt.request.request_id,
            sub_agent_id: Some(sub_agent_id.to_string()),
            decision: decision.clone(),
            decided_by,
            decided_at: durable_utc_now(ctx).await?,
        },
    )
    .await?;

    match decision {
        ApprovalDecision::AllowOnce => Ok(ApprovalOutcome::allow_execution()),
        ApprovalDecision::AlwaysAllow { pattern } => {
            ctx.service_client::<WorkspaceStoreClient>()
                .store_approval_rule(Json(StoreApprovalRuleRequest {
                    session: meta.clone(),
                    tool_name: invocation.name.clone(),
                    pattern,
                    action: PolicyAction::Allow,
                    created_by: meta.user_id.clone(),
                }))
                .call()
                .await?;
            Ok(ApprovalOutcome::allow_execution())
        }
        ApprovalDecision::Deny { reason } => {
            let message = reason.unwrap_or_else(|| "Denied by the user".to_string());
            Ok(ApprovalOutcome::deny(denied_tool_output(format!(
                "Tool execution denied: {message}"
            ))))
        }
    }
}

async fn cleanup_pending_approval_after_cancel(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    sub_agent_id: &str,
    reason: &str,
) -> Result<(), HandlerError> {
    let Some(pending) = ctx
        .get::<Json<PendingApprovalState>>(K_PENDING_APPROVAL)
        .await?
        .map(Json::into_inner)
    else {
        return Ok(());
    };

    let decision = ApprovalDecision::Deny {
        reason: Some(approval_wait::cancel_reason(reason)),
    };
    let serialized = serialize_awakeable_decision(&decision)?;
    ctx.resolve_awakeable(&pending.awakeable_id, serialized);
    ctx.clear(K_PENDING_APPROVAL);
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .clear_pending_approval(Json::from(ClearSubAgentPendingApprovalInput {
            turn_id: turn_id.to_string(),
        }))
        .call()
        .await?;
    append_session_event(
        ctx,
        pending.session_id,
        Event::ApprovalDecided {
            request_id: pending.request_id,
            sub_agent_id: Some(pending.sub_agent_id),
            decision,
            decided_by: "system:cancel".to_string(),
            decided_at: durable_utc_now(ctx).await?,
        },
    )
    .await?;
    Ok(())
}

async fn record_tool_result(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    sub_agent_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_tool_result(Json::from(SubAgentToolRecord {
            turn_id: Some(turn_id.to_string()),
            tool_id,
            invocation: invocation.clone(),
            output: output.clone(),
        }))
        .call()
        .await?;
    Ok(())
}

async fn record_denied_tool(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    sub_agent_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_denied_tool(Json::from(SubAgentToolRecord {
            turn_id: Some(turn_id.to_string()),
            tool_id,
            invocation: invocation.clone(),
            output: output.clone(),
        }))
        .call()
        .await?;
    Ok(())
}

async fn record_segment_tool_use(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_name: &str,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
            session_id,
            tool_name: tool_name.to_string(),
        }))
        .send();
    Ok(())
}

async fn append_tool_call_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_session_event(
        ctx,
        session_id,
        Event::ToolCall {
            tool_id,
            provider_tool_use_id: invocation.id,
            provider_thought_signature: tool_call
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.thought_signature())
                .map(str::to_string),
            tool_name: invocation.name,
            input: invocation.input,
            hand_id: None,
        },
    )
    .await
    .map(|_| ())
}

async fn append_tool_result_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        session_id,
        Event::ToolResult {
            tool_id,
            provider_tool_use_id: invocation.id.clone(),
            output: output.clone(),
            original_output_tokens: output.original_output_tokens,
            success: false,
            duration_ms: 0,
        },
    )
    .await
    .map(|_| ())
}

async fn append_delegation_tool_result(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        session_id,
        Event::ToolResult {
            tool_id,
            provider_tool_use_id: invocation.id.clone(),
            output: output.clone(),
            original_output_tokens: output.original_output_tokens,
            success: !output.is_error,
            duration_ms: 0,
        },
    )
    .await
    .map(|_| ())
}

async fn append_session_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<u64, HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    let sequence_num = ctx
        .service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(sequence_num)
}

async fn cancel_requested(ctx: &WorkflowContext<'_>) -> Result<Option<String>, HandlerError> {
    Ok(meaningful_cancel_reason(
        ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE).await?,
    ))
}

fn notify_sub_agent_of_outcome(
    ctx: &WorkflowContext<'_>,
    sub_agent_id: &str,
    outcome: &TurnOutcome,
) {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_turn_outcome(Json::from(outcome.clone()))
        .send();
}

fn workflow_outcome_from_core(
    request: &RunSubAgentTurnRequest,
    outcome: CoreTurnOutcome,
) -> TurnOutcome {
    match outcome {
        CoreTurnOutcome::Continue | CoreTurnOutcome::Idle | CoreTurnOutcome::WaitingApproval => {
            TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Completed,
                message: match outcome {
                    CoreTurnOutcome::Continue => "sub-agent turn yielded continuation".to_string(),
                    CoreTurnOutcome::Idle => "sub-agent turn completed".to_string(),
                    CoreTurnOutcome::WaitingApproval => {
                        "sub-agent turn is waiting for approval".to_string()
                    }
                    CoreTurnOutcome::Cancelled => unreachable!(),
                },
            }
        }
        CoreTurnOutcome::Cancelled => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Cancelled,
            message: "sub-agent turn cancelled".to_string(),
        },
    }
}

async fn durable_utc_now(ctx: &WorkflowContext<'_>) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("sub_agent_workflow_utc_now")
        .await?
        .into_inner())
}

fn is_terminal_phase(phase: &TurnPhase) -> bool {
    matches!(
        phase,
        TurnPhase::Completed | TurnPhase::Cancelled | TurnPhase::Failed
    )
}

#[cfg(test)]
mod tests {
    use moa_core::wire::TurnPhase;
    use moa_core::{ApprovalDecision, SessionId};

    use crate::turn::approval::parse_awakeable_decision;

    use super::{is_terminal_phase, parent_session_from_initial_message};

    #[test]
    fn terminal_phase_detection_matches_workflow_lifecycle() {
        // Pins: cancellation requests stop mutating completed sub-agent workflows.
        assert!(!is_terminal_phase(&TurnPhase::Pending));
        assert!(!is_terminal_phase(&TurnPhase::Compiling));
        assert!(!is_terminal_phase(&TurnPhase::Streaming));
        assert!(!is_terminal_phase(&TurnPhase::Tooling));
        assert!(!is_terminal_phase(&TurnPhase::Persisting));
        assert!(is_terminal_phase(&TurnPhase::Completed));
        assert!(is_terminal_phase(&TurnPhase::Cancelled));
        assert!(is_terminal_phase(&TurnPhase::Failed));
    }

    #[test]
    fn awakeable_decision_round_trips_through_json_payload() {
        // Pins: SubAgent::approve payloads remain compatible with workflow approval parsing.
        let encoded =
            crate::turn::approval::serialize_awakeable_decision(&ApprovalDecision::AlwaysAllow {
                pattern: "bash:npm test".to_string(),
            })
            .expect("serialize approval decision");

        let decoded = parse_awakeable_decision(&encoded).expect("deserialize approval decision");

        assert_eq!(
            decoded,
            ApprovalDecision::AlwaysAllow {
                pattern: "bash:npm test".to_string()
            }
        );
    }

    #[test]
    fn reserved_child_parent_session_requires_initial_message() {
        // Pins: nested spawn events derive their root session only from validated initial child messages.
        let session_id = SessionId::new();
        let message = moa_core::SubAgentMessage::InitialTask {
            task: "inspect".to_string(),
            tool_subset: Vec::new(),
            budget_tokens: 100,
            parent_session: session_id,
            parent_sub_agent: Some("parent".to_string()),
            depth: 2,
            result_awakeable_id: String::new(),
            workspace_id: moa_core::WorkspaceId::new("workspace"),
            user_id: moa_core::UserId::new("user"),
            model: moa_core::ModelId::new("model"),
        };

        assert_eq!(
            parent_session_from_initial_message(&message)
                .expect("initial task should expose parent session"),
            session_id
        );
        let error = parent_session_from_initial_message(&moa_core::SubAgentMessage::FollowUp {
            text: "continue".to_string(),
        })
        .expect_err("follow-up messages should not be accepted as reservations");
        assert!(format!("{error:?}").contains("initial task message"));
    }
}
