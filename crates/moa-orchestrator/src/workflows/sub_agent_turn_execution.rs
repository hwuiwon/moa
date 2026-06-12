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
    RunSubAgentTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{
    ApprovalDecision, ApprovalPrompt, CancelSubAgentInput, CompleteSubAgentChildInput,
    CompletionRequest, DispatchSubAgentInput, Event, ListSubAgentsInput, ListSubAgentsOutput,
    MessageSubAgentInput, PolicyAction, ReserveSubAgentInput, ReservedSubAgent, SessionId,
    SessionMeta, SpawnSubAgentInput, SpawnSubAgentOutput, SubAgentChildRef, SubAgentMessage,
    SubAgentResult, SubAgentState, SubAgentToolRecord, SubAgentTurnPreparation, ToolCallContent,
    ToolCallId, ToolCallRequest, ToolInvocation, ToolOutput, TurnOutcome as CoreTurnOutcome,
    WaitSubAgentInput, WaitSubAgentOutput, is_delegation_tool_name, record_approval_wait,
    record_turn_event_persist_duration, record_turn_llm_call_duration,
    record_turn_tool_dispatch_duration,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::objects::sub_agent::{MAX_SUB_AGENT_TURNS_PER_WORKFLOW, SubAgentClient};
use crate::services::{
    llm_gateway::LLMGatewayClient,
    session_store::{AppendEventRequest, RecordSegmentToolUseRequest, RestateSessionStoreClient},
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareToolApprovalRequest, StoreApprovalRuleRequest, WorkspaceStoreClient},
};
use crate::sub_agent_dispatch::{DispatchedSubAgent, child_is_owned, sub_agent_result_tool_output};
use crate::turn::approval::{parse_awakeable_decision, serialize_awakeable_decision};
use crate::turn::util::{
    allowed_tool_names, denied_tool_output, disallowed_tool_output, meaningful_cancel_reason,
    response_tool_calls, stable_tool_call_id, tool_call_is_allowed, turn_outcome_for_response,
};

const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_PHASE: &str = "phase";
const APPROVAL_TIMEOUT_SECS_ENV: &str = "MOA_APPROVAL_TIMEOUT_SECS";
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;

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
            cleanup_pending_approval_after_cancel(&ctx, &request.sub_agent_id, &outcome.message)
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
        let (mut completion_request, meta, parent_session) = match preparation {
            SubAgentTurnPreparation::Outcome { outcome } => {
                return Ok(workflow_outcome_from_core(request, outcome));
            }
            SubAgentTurnPreparation::Request {
                request,
                session_meta,
                parent_session,
            } => (*request, *session_meta, parent_session),
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
            .record_response(Json::from(response.clone()))
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
            handle_tool_call(
                ctx,
                &request.sub_agent_id,
                &meta,
                parent_session,
                &allowed_tools,
                index,
                tool_call,
            )
            .await?;
        }

        let outcome = turn_outcome_for_response(&response);
        ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
            .apply_turn_outcome(Json::from(outcome))
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

async fn handle_tool_call(
    ctx: &WorkflowContext<'_>,
    sub_agent_id: &str,
    meta: &SessionMeta,
    session_id: SessionId,
    allowed_tools: &BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    ctx.set(K_PHASE, Json::from(TurnPhase::Tooling));
    let tool_id = stable_tool_call_id(session_id, index, tool_call);
    let invocation = tool_call.invocation.clone();

    if !tool_call_is_allowed(allowed_tools, &invocation.name) {
        append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
        let output = disallowed_tool_output(&invocation.name);
        append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
        record_denied_tool(ctx, sub_agent_id, tool_id, &invocation, &output).await?;
        return Ok(());
    }

    if invocation.name == "dispatch_sub_agent" {
        handle_dispatch(ctx, sub_agent_id, session_id, tool_id, tool_call).await?;
        return Ok(());
    }

    if is_delegation_tool_name(&invocation.name) {
        handle_delegation_tool(ctx, sub_agent_id, session_id, tool_id, tool_call).await?;
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
        record_denied_tool(ctx, sub_agent_id, tool_id, &invocation, &output).await?;
        return Ok(());
    }

    if matches!(policy.action, PolicyAction::RequireApproval) {
        let decided = handle_approval_gate(
            ctx,
            sub_agent_id,
            session_id,
            meta,
            &invocation,
            tool_id,
            policy.prompt,
        )
        .await?;
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

    record_tool_result(ctx, sub_agent_id, tool_id, &invocation, &output).await?;
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn handle_dispatch(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    let dispatch_input: DispatchSubAgentInput = serde_json::from_value(invocation.input.clone())
        .map_err(|error| {
            TerminalError::new(format!(
                "failed to deserialize dispatch_sub_agent input: {error}"
            ))
        })?;

    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let span = tool_dispatch_span("dispatch_sub_agent");
    let dispatch_started = Instant::now();
    let dispatched = dispatch_child(ctx, parent_sub_agent_id, dispatch_input)
        .instrument(span)
        .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    let output = sub_agent_result_tool_output(&dispatched.result);
    append_delegation_tool_result(ctx, session_id, tool_id, &invocation, &output).await?;
    record_tool_result(ctx, parent_sub_agent_id, tool_id, &invocation, &output).await?;
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = match invocation.name.as_str() {
        "spawn_sub_agent" => {
            let input: SpawnSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize spawn_sub_agent input: {error}"
                    ))
                })?;
            let output = spawn_child_detached(ctx, parent_sub_agent_id, input)
                .instrument(span)
                .await?;
            crate::delegation::spawn_output(output)
        }
        "wait_sub_agent" => {
            let input: WaitSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize wait_sub_agent input: {error}"
                    ))
                })?;
            crate::delegation::wait_output(
                wait_child(ctx, parent_sub_agent_id, input)
                    .instrument(span)
                    .await?,
            )
        }
        "message_sub_agent" => {
            let input: MessageSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize message_sub_agent input: {error}"
                    ))
                })?;
            let sub_agent_id = input.sub_agent_id.clone();
            message_child(ctx, parent_sub_agent_id, session_id, input)
                .instrument(span)
                .await?;
            crate::delegation::message_output(&sub_agent_id)
        }
        "list_sub_agents" => {
            let input: ListSubAgentsInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize list_sub_agents input: {error}"
                    ))
                })?;
            crate::delegation::list_output(
                list_children(ctx, parent_sub_agent_id, input)
                    .instrument(span)
                    .await?,
            )
        }
        "cancel_sub_agent" => {
            let input: CancelSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize cancel_sub_agent input: {error}"
                    ))
                })?;
            let sub_agent_id = input.sub_agent_id.clone();
            cancel_child(ctx, parent_sub_agent_id, session_id, input)
                .instrument(span)
                .await?;
            crate::delegation::cancel_output(&sub_agent_id)
        }
        _ => {
            return Err(TerminalError::new(format!(
                "unsupported delegation tool {}",
                invocation.name
            ))
            .into());
        }
    };
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_delegation_tool_result(ctx, session_id, tool_id, &invocation, &output).await?;
    record_tool_result(ctx, parent_sub_agent_id, tool_id, &invocation, &output).await?;
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn dispatch_child(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    request: DispatchSubAgentInput,
) -> Result<DispatchedSubAgent, HandlerError> {
    let (awakeable_id, result_future) = ctx.awakeable::<String>();
    let reservation = reserve_child(
        ctx,
        parent_sub_agent_id,
        ReserveSubAgentInput {
            request,
            task_name: None,
            result_awakeable_id: awakeable_id,
        },
    )
    .await?;
    start_reserved_child(ctx, &reservation);
    append_child_spawned_event(ctx, parent_sub_agent_id, &reservation).await?;

    let result = parse_sub_agent_result(&result_future.await?)?;
    complete_child(
        ctx,
        parent_sub_agent_id,
        &reservation.child_ref.id,
        result.tokens_used,
    )
    .await?;

    Ok(DispatchedSubAgent {
        id: reservation.child_ref.id,
        result,
    })
}

async fn spawn_child_detached(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    request: SpawnSubAgentInput,
) -> Result<SpawnSubAgentOutput, HandlerError> {
    let dispatch_request = DispatchSubAgentInput::from(request.clone());
    let reservation = reserve_child(
        ctx,
        parent_sub_agent_id,
        ReserveSubAgentInput {
            request: dispatch_request,
            task_name: request.task_name.clone(),
            result_awakeable_id: String::new(),
        },
    )
    .await?;
    start_reserved_child(ctx, &reservation);
    append_child_spawned_event(ctx, parent_sub_agent_id, &reservation).await?;

    Ok(SpawnSubAgentOutput {
        sub_agent_id: reservation.child_ref.id,
        path: reservation.path,
        status: SubAgentState::Running,
    })
}

async fn wait_child(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    input: WaitSubAgentInput,
) -> Result<WaitSubAgentOutput, HandlerError> {
    let timeout_ms = crate::delegation::clamp_wait_timeout_ms(input.timeout_ms);
    let mut waited_ms = 0;
    ensure_child_owned(ctx, parent_sub_agent_id, &input.sub_agent_id).await?;
    loop {
        let status = ctx
            .object_client::<SubAgentClient>(input.sub_agent_id.clone())
            .status()
            .call()
            .await?
            .into_inner();
        if crate::delegation::is_terminal_sub_agent_state(status.state) {
            let result = ctx
                .object_client::<SubAgentClient>(input.sub_agent_id.clone())
                .result()
                .call()
                .await?
                .into_inner()
                .ok_or_else(|| TerminalError::new("terminal sub-agent result missing"))?;
            complete_child(
                ctx,
                parent_sub_agent_id,
                &input.sub_agent_id,
                result.tokens_used,
            )
            .await?;
            return Ok(WaitSubAgentOutput {
                sub_agent_id: input.sub_agent_id,
                state: status.state,
                result: Some(result),
                timed_out: false,
            });
        }

        if waited_ms >= timeout_ms {
            return Ok(WaitSubAgentOutput {
                sub_agent_id: input.sub_agent_id,
                state: status.state,
                result: None,
                timed_out: true,
            });
        }

        let sleep_ms = crate::delegation::WAIT_POLL_INTERVAL_MS.min(timeout_ms - waited_ms);
        ctx.sleep(Duration::from_millis(sleep_ms)).await?;
        waited_ms += sleep_ms;
    }
}

async fn message_child(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    input: MessageSubAgentInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent_sub_agent_id, &input.sub_agent_id).await?;
    let sub_agent_id = input.sub_agent_id.clone();
    ctx.object_client::<SubAgentClient>(sub_agent_id.clone())
        .post_message(Json::from(SubAgentMessage::FollowUp {
            text: input.text.clone(),
        }))
        .send();
    append_session_event(
        ctx,
        session_id,
        Event::SubAgentMessageSent {
            sub_agent_id,
            parent_sub_agent_id: Some(parent_sub_agent_id.to_string()),
            text: input.text,
        },
    )
    .await?;
    Ok(())
}

async fn list_children(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    _input: ListSubAgentsInput,
) -> Result<ListSubAgentsOutput, HandlerError> {
    let children = child_refs(ctx, parent_sub_agent_id).await?;
    let mut sub_agents = Vec::with_capacity(children.len());
    for child in children {
        let status = ctx
            .object_client::<SubAgentClient>(child.id.clone())
            .status()
            .call()
            .await?
            .into_inner();
        sub_agents.push(crate::delegation::listed_sub_agent(child.id, status));
    }
    Ok(ListSubAgentsOutput { sub_agents })
}

async fn cancel_child(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    input: CancelSubAgentInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent_sub_agent_id, &input.sub_agent_id).await?;
    let sub_agent_id = input.sub_agent_id.clone();
    ctx.object_client::<SubAgentClient>(input.sub_agent_id)
        .cancel(input.reason)
        .send();
    append_session_event(
        ctx,
        session_id,
        Event::SubAgentStatusChanged {
            sub_agent_id,
            from: None,
            to: SubAgentState::Cancelled,
            summary: Some("cancel requested by parent sub-agent".to_string()),
        },
    )
    .await?;
    Ok(())
}

async fn reserve_child(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    input: ReserveSubAgentInput,
) -> Result<ReservedSubAgent, HandlerError> {
    Ok(ctx
        .object_client::<SubAgentClient>(parent_sub_agent_id.to_string())
        .reserve_child(Json::from(input))
        .call()
        .await?
        .into_inner())
}

fn start_reserved_child(ctx: &WorkflowContext<'_>, reservation: &ReservedSubAgent) {
    ctx.object_client::<SubAgentClient>(reservation.child_ref.id.clone())
        .post_message(Json::from(reservation.initial_message.clone()))
        .send();
}

async fn append_child_spawned_event(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    reservation: &ReservedSubAgent,
) -> Result<(), HandlerError> {
    let parent_session = parent_session_from_initial_message(&reservation.initial_message)?;
    append_session_event(
        ctx,
        parent_session,
        Event::SubAgentSpawned {
            sub_agent_id: reservation.child_ref.id.clone(),
            parent_sub_agent_id: Some(parent_sub_agent_id.to_string()),
            path: reservation.path.clone(),
            task: reservation.task.clone(),
            budget_tokens: reservation.budget_tokens,
        },
    )
    .await?;
    Ok(())
}

fn parent_session_from_initial_message(
    message: &SubAgentMessage,
) -> Result<SessionId, HandlerError> {
    match message {
        SubAgentMessage::InitialTask { parent_session, .. } => Ok(*parent_session),
        SubAgentMessage::FollowUp { .. } | SubAgentMessage::ChildResult { .. } => {
            Err(TerminalError::new("reserved child did not include an initial task message").into())
        }
    }
}

async fn complete_child(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    sub_agent_id: &str,
    tokens_used: u64,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(parent_sub_agent_id.to_string())
        .complete_child(Json::from(CompleteSubAgentChildInput {
            sub_agent_id: sub_agent_id.to_string(),
            tokens_used,
        }))
        .call()
        .await?;
    Ok(())
}

async fn ensure_child_owned(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
    sub_agent_id: &str,
) -> Result<(), HandlerError> {
    let children = child_refs(ctx, parent_sub_agent_id).await?;
    if child_is_owned(&children, sub_agent_id) {
        return Ok(());
    }
    Err(TerminalError::new(format!(
        "sub-agent {sub_agent_id} is not owned by this parent"
    ))
    .into())
}

async fn child_refs(
    ctx: &WorkflowContext<'_>,
    parent_sub_agent_id: &str,
) -> Result<Vec<SubAgentChildRef>, HandlerError> {
    Ok(ctx
        .object_client::<SubAgentClient>(parent_sub_agent_id.to_string())
        .child_refs()
        .call()
        .await?
        .into_inner())
}

async fn handle_approval_gate(
    ctx: &WorkflowContext<'_>,
    sub_agent_id: &str,
    session_id: SessionId,
    meta: &SessionMeta,
    invocation: &ToolInvocation,
    tool_id: ToolCallId,
    prompt: Option<ApprovalPrompt>,
) -> Result<ApprovalOutcome, HandlerError> {
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
        .set_pending_approval(Json::from(awakeable_id.clone()))
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

    let approval_timeout = approval_wait_timeout();
    let timed_out_reason = format!(
        "Auto-denied: no decision within {} minutes",
        approval_timeout.as_secs() / 60
    );
    let approval_started = Instant::now();
    let decision = restate_sdk::select! {
        decision = awakeable => {
            parse_awakeable_decision(&decision?)?
        },
        reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
            ApprovalDecision::Deny {
                reason: Some(format!("Cancelled while waiting for approval: {}", reason?)),
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
        approval_outcome_label(&decision, &timed_out_reason),
    );

    ctx.clear(K_PENDING_APPROVAL);
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .clear_pending_approval()
        .call()
        .await?;

    let decided_by = match &decision {
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason == &timed_out_reason => "system:auto-timeout".to_string(),
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason.starts_with("Cancelled while waiting for approval:") => {
            "system:cancel".to_string()
        }
        _ => meta.user_id.to_string(),
    };

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
        reason: Some(format!("Cancelled while waiting for approval: {reason}")),
    };
    let serialized = serialize_awakeable_decision(&decision)?;
    ctx.resolve_awakeable(&pending.awakeable_id, serialized);
    ctx.clear(K_PENDING_APPROVAL);
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .clear_pending_approval()
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
    sub_agent_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_tool_result(Json::from(SubAgentToolRecord {
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
    sub_agent_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_denied_tool(Json::from(SubAgentToolRecord {
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

fn parse_sub_agent_result(raw: &str) -> Result<SubAgentResult, HandlerError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize sub-agent result from awakeable: {error}"
        ))
        .into()
    })
}

fn approval_wait_timeout() -> Duration {
    approval_wait_timeout_from_env(
        std::env::var(APPROVAL_TIMEOUT_SECS_ENV).ok().as_deref(),
        DEFAULT_APPROVAL_TIMEOUT_SECS,
    )
}

fn approval_wait_timeout_from_env(raw: Option<&str>, default_secs: u64) -> Duration {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

fn approval_outcome_label<'a>(
    decision: &'a ApprovalDecision,
    timed_out_reason: &'a str,
) -> &'a str {
    match decision {
        ApprovalDecision::AllowOnce => "allow_once",
        ApprovalDecision::AlwaysAllow { .. } => "always_allow",
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason == timed_out_reason => "timeout",
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason.starts_with("Cancelled while waiting for approval:") => "cancel",
        ApprovalDecision::Deny { .. } => "deny",
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
    use std::time::Duration;

    use moa_core::wire::TurnPhase;
    use moa_core::{ApprovalDecision, SessionId};

    use crate::turn::approval::parse_awakeable_decision;

    use super::{
        approval_outcome_label, approval_wait_timeout_from_env, is_terminal_phase,
        parent_session_from_initial_message,
    };

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
    fn approval_timeout_defaults_when_override_is_missing_or_invalid() {
        // Pins: sub-agent approval gates cannot disable timeout with bad env overrides.
        assert_eq!(
            approval_wait_timeout_from_env(None, 1800),
            Duration::from_secs(1800)
        );
        assert_eq!(
            approval_wait_timeout_from_env(Some("not-a-number"), 1800),
            Duration::from_secs(1800)
        );
        assert_eq!(
            approval_wait_timeout_from_env(Some("0"), 1800),
            Duration::from_secs(1800)
        );
        assert_eq!(
            approval_wait_timeout_from_env(Some("45"), 1800),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn approval_outcome_labels_distinguish_cancel_from_timeout() {
        // Pins: approval wait metrics classify cancellation separately from user denial and timeout.
        let timed_out_reason = "Auto-denied: no decision within 30 minutes";
        assert_eq!(
            approval_outcome_label(
                &ApprovalDecision::Deny {
                    reason: Some(timed_out_reason.to_string())
                },
                timed_out_reason
            ),
            "timeout"
        );
        assert_eq!(
            approval_outcome_label(
                &ApprovalDecision::Deny {
                    reason: Some("Cancelled while waiting for approval: stop".to_string())
                },
                timed_out_reason
            ),
            "cancel"
        );
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
