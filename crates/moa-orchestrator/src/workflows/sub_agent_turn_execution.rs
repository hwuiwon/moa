//! Workflow-backed execution for one sub-agent turn run.
//!
//! The `SubAgent` virtual object owns conversational state and message
//! admission. This workflow owns the repeated LLM/tool loop so `post_message`
//! can return quickly and child execution has a durable progress/cancellation
//! surface like top-level session turns.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use moa_core::restate_observability::{
    annotate_restate_handler_span, event_persist_span, llm_call_span, sub_agent_turn_span,
    tool_dispatch_span,
};
use moa_core::wire::{
    AppendEventRequest, RecordSegmentToolUseRequest, RunSubAgentTurnRequest, TurnOutcome,
    TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{
    ActionPolicyEffect, CompletionRequest, Event, ModelTier, SessionId, SessionMeta,
    SubAgentToolRecord, SubAgentTurnOutcomeRecord, SubAgentTurnPreparation,
    SubAgentTurnResponseRecord, ToolCallContent, ToolCallId, ToolCallRequest, ToolInvocation,
    ToolOutput, TurnOutcome as CoreTurnOutcome, is_delegation_tool_name,
    record_turn_event_persist_duration, record_turn_llm_call_duration,
    record_turn_tool_dispatch_duration, record_turn_workflow_outcome,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::objects::sub_agent::{MAX_SUB_AGENT_TURNS_PER_WORKFLOW, SubAgentClient};
use crate::services::{
    action_reviews::{ActionReviewsClient, RequestActionReview},
    llm_gateway::LLMGatewayClient,
    session_store::RestateSessionStoreClient,
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareActionReviewRequest, WorkspaceStoreClient},
};
use crate::turn::util::{
    allowed_tool_names, blocked_canary_tool_output, denied_tool_output, disallowed_tool_output,
    meaningful_cancel_reason, response_tool_calls, stable_tool_call_id, tool_call_is_allowed,
    tool_input_leaks_canary, turn_outcome_for_response,
};

const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const K_PHASE: &str = "phase";

#[derive(Clone, Debug)]
enum SubAgentIterationOutcome {
    Core(CoreTurnOutcome),
    Cancelled(String),
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

        let workflow_started = Instant::now();
        let outcome = match run_sub_agent_inside_workflow(&ctx, &request).await {
            Ok(outcome) => outcome,
            Err(error) => TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Failed,
                message: format!("{error:?}"),
            },
        };
        record_turn_workflow_outcome(
            "sub_agent",
            turn_outcome_kind_label(&outcome.kind),
            ModelTier::Auxiliary,
            workflow_started.elapsed(),
        );
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
    let max_turns = match effective_sub_agent_max_turns(request.max_turns) {
        Ok(max_turns) => max_turns,
        Err(message) => {
            return Ok(TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Failed,
                message,
            });
        }
    };
    for turn_number in 1..=max_turns {
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
        let (completion_request, active_canary, meta, parent_session) = match preparation {
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
        let turn_span = sub_agent_turn_span(
            &meta,
            &request.sub_agent_id,
            &request.turn_id,
            turn_number as i64,
            None,
        );
        let outcome = run_sub_agent_iteration(
            ctx,
            request,
            completion_request,
            active_canary,
            meta,
            parent_session,
        )
        .instrument(turn_span)
        .await?;
        match outcome {
            SubAgentIterationOutcome::Cancelled(message) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message,
                });
            }
            SubAgentIterationOutcome::Core(CoreTurnOutcome::Continue) => continue,
            SubAgentIterationOutcome::Core(CoreTurnOutcome::Idle) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message: "sub-agent turn completed".to_string(),
                });
            }
            SubAgentIterationOutcome::Core(CoreTurnOutcome::Cancelled) => {
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
        message: format!("sub-agent turn budget exceeded ({max_turns})"),
    })
}

fn effective_sub_agent_max_turns(max_turns: Option<u32>) -> Result<usize, String> {
    match max_turns {
        Some(0) => Err("sub-agent max_turns must be at least 1".to_string()),
        Some(max_turns) => Ok(max_turns as usize),
        None => Ok(MAX_SUB_AGENT_TURNS_PER_WORKFLOW),
    }
}

async fn run_sub_agent_iteration(
    ctx: &WorkflowContext<'_>,
    request: &RunSubAgentTurnRequest,
    mut completion_request: CompletionRequest,
    active_canary: Option<String>,
    meta: SessionMeta,
    parent_session: SessionId,
) -> Result<SubAgentIterationOutcome, HandlerError> {
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
                return Ok(SubAgentIterationOutcome::Cancelled(reason));
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
            return Ok(SubAgentIterationOutcome::Cancelled(reason));
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
    Ok(SubAgentIterationOutcome::Core(outcome))
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

    let prepared_action = ctx
        .service_client::<WorkspaceStoreClient>()
        .prepare_action_review(Json(PrepareActionReviewRequest {
            session: meta.clone(),
            invocation: invocation.clone(),
            review_id: tool_id.0,
            tool_call_id: tool_id,
            sub_agent_id: Some(sub_agent_id.to_string()),
            origin_kind: Some("sub_agent".to_string()),
            origin_id: Some(sub_agent_id.to_string()),
            origin_step_id: Some(tool_context.turn_id.to_string()),
            idempotency_key: invocation.id.clone(),
        }))
        .call()
        .await?
        .into_inner();

    if matches!(prepared_action.effect, ActionPolicyEffect::Deny) {
        let reason = prepared_action
            .reason
            .as_deref()
            .unwrap_or("denied by action policy");
        let output = denied_tool_output(format!(
            "Tool {} denied by action policy: {reason}",
            invocation.name
        ));
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

    if matches!(prepared_action.effect, ActionPolicyEffect::AdminReview) {
        let tool_request = ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            active_canary: tool_context.active_canary.map(ToOwned::to_owned),
            session_id: Some(session_id),
            tenant_id: meta.tenant_id,
            user_id: storage_user_id(meta),
            idempotency_key: invocation.id.clone(),
        };
        if tool_input_leaks_canary(tool_context.active_canary, &tool_request.input)
            .map_err(|error| TerminalError::new(format!("serialize tool input: {error}")))?
        {
            let output = blocked_canary_tool_output(&invocation.name);
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
        ctx.service_client::<ActionReviewsClient>()
            .request(Json::from(RequestActionReview {
                envelope: prepared_action.envelope,
                preview: prepared_action.preview,
                tool_request,
            }))
            .call()
            .await?;
        let output = ToolOutput::error(
            format!(
                "Action is pending tenant admin review: {}: {}",
                invocation.name, prepared_action.input_summary
            ),
            Duration::ZERO,
        );
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
            tenant_id: meta.tenant_id,
            user_id: storage_user_id(meta),
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
        moa_core::SubAgentMessage::FollowUp { .. } => {
            Err(TerminalError::new("reserved child did not include an initial task message").into())
        }
    }
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
        CoreTurnOutcome::Continue | CoreTurnOutcome::Idle => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Completed,
            message: match outcome {
                CoreTurnOutcome::Continue => "sub-agent turn yielded continuation".to_string(),
                CoreTurnOutcome::Idle => "sub-agent turn completed".to_string(),
                CoreTurnOutcome::Cancelled => unreachable!(),
            },
        },
        CoreTurnOutcome::Cancelled => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Cancelled,
            message: "sub-agent turn cancelled".to_string(),
        },
    }
}

fn is_terminal_phase(phase: &TurnPhase) -> bool {
    matches!(
        phase,
        TurnPhase::Completed | TurnPhase::Cancelled | TurnPhase::Failed
    )
}

fn turn_outcome_kind_label(kind: &TurnOutcomeKind) -> &'static str {
    match kind {
        TurnOutcomeKind::Completed => "completed",
        TurnOutcomeKind::Cancelled => "cancelled",
        TurnOutcomeKind::Failed => "failed",
    }
}

fn storage_user_id(meta: &SessionMeta) -> moa_core::UserId {
    let value = meta
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .or_else(|| meta.created_by.as_ref().map(session_actor_storage_id))
        .unwrap_or_else(|| format!("tenant:{}", meta.tenant_id));
    moa_core::UserId::new(value)
}

fn session_actor_storage_id(actor: &moa_core::SessionActorRef) -> String {
    match actor {
        moa_core::SessionActorRef::Identity { id } => format!("identity:{id}"),
        moa_core::SessionActorRef::Contact { id } => id.to_string(),
        moa_core::SessionActorRef::Anonymous => "anonymous".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::SessionId;
    use moa_core::wire::TurnPhase;

    use super::{
        MAX_SUB_AGENT_TURNS_PER_WORKFLOW, effective_sub_agent_max_turns, is_terminal_phase,
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
    fn effective_sub_agent_max_turns_honors_request_cap() {
        // Pins: workflow sub_agent max_turns is applied by the child turn loop.
        assert_eq!(
            effective_sub_agent_max_turns(None).expect("default cap should be valid"),
            MAX_SUB_AGENT_TURNS_PER_WORKFLOW
        );
        assert_eq!(
            effective_sub_agent_max_turns(Some(3)).expect("explicit cap should be valid"),
            3
        );
        assert!(
            effective_sub_agent_max_turns(Some(0))
                .expect_err("zero cap should fail closed")
                .contains("max_turns must be at least 1")
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
            max_turns: Some(2),
            parent_session: session_id,
            parent_sub_agent: Some("parent".to_string()),
            depth: 2,
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
