//! Single-turn brain harness execution and the shared streamed turn engine.

use std::sync::Arc;

use moa_core::{
    ApprovalDecision, ApprovalPrompt, ApprovalRequest, CompletionContent, Event, EventRange,
    EventRecord, LLMProvider, MoaError, PolicyAction, Result, RuntimeEvent, SessionId, SessionMeta,
    SessionSignal, SessionStatus, SessionStore, StopReason, ToolCardStatus, ToolInvocation,
    ToolUpdate, UserId, UserMessage, WorkingContext,
};
use moa_hands::ToolRouter;
use moa_security::{
    InputClassification, check_canary, contains_canary_tokens, inject_canary, inspect_input,
};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::pipeline::ContextPipeline;
use crate::turn::{
    PendingToolApproval, StoredApprovalDecision, StreamSignalDisposition,
    find_pending_approval_request, find_pending_tool_approval, find_resolved_pending_tool_approval,
    stream_completion_response,
};

/// Outcome of a single buffered brain turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    /// The session has produced a final response for this turn.
    Complete,
    /// The session should continue in another turn.
    Continue,
    /// The session is blocked waiting for an approval decision.
    NeedsApproval(ApprovalRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolLoopMode {
    LoopUntilTurnBoundary,
    StepAfterToolBoundary,
}

/// Outcome of the shared streamed turn engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamedTurnResult {
    /// The session completed a full assistant turn.
    Complete,
    /// The session should immediately continue with another turn.
    Continue,
    /// The session is blocked waiting for approval.
    NeedsApproval(ApprovalRequest),
    /// The turn was cancelled before completion.
    Cancelled,
}

/// Runs one buffered turn of the brain harness.
pub async fn run_brain_turn(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
) -> Result<TurnResult> {
    run_brain_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        None,
        ToolLoopMode::LoopUntilTurnBoundary,
    )
    .await
}

/// Runs one buffered turn of the brain harness with optional tool execution support.
pub async fn run_brain_turn_with_tools(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
) -> Result<TurnResult> {
    run_brain_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        ToolLoopMode::LoopUntilTurnBoundary,
    )
    .await
}

/// Runs one buffered turn of the brain harness, yielding after any tool boundary.
pub async fn run_brain_turn_with_tools_stepwise(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
) -> Result<TurnResult> {
    run_brain_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        ToolLoopMode::StepAfterToolBoundary,
    )
    .await
}

/// Runs the shared streamed turn engine without live session signals.
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_turn(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<StreamedTurnResult> {
    run_streamed_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        runtime_tx,
        event_tx,
        cancel_token,
        hard_cancel_token,
        None,
        None,
        None,
        None,
        ToolLoopMode::LoopUntilTurnBoundary,
    )
    .await
}

/// Runs the shared streamed turn engine while consuming live session signals.
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_turn_with_signals(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<StreamedTurnResult> {
    run_streamed_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        runtime_tx,
        event_tx,
        cancel_token,
        hard_cancel_token,
        Some(signal_rx),
        Some(turn_requested),
        Some(queued_messages),
        Some(soft_cancel_requested),
        ToolLoopMode::LoopUntilTurnBoundary,
    )
    .await
}

async fn run_brain_turn_with_tools_mode(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    tool_loop_mode: ToolLoopMode,
) -> Result<TurnResult> {
    let (runtime_tx, _) = broadcast::channel(256);
    let streamed = run_streamed_turn_with_tools_mode(
        session_id,
        session_store,
        llm_provider,
        pipeline,
        tool_router,
        &runtime_tx,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        tool_loop_mode,
    )
    .await?;

    match streamed {
        StreamedTurnResult::Complete => Ok(TurnResult::Complete),
        StreamedTurnResult::Continue => Ok(TurnResult::Continue),
        StreamedTurnResult::NeedsApproval(request) => Ok(TurnResult::NeedsApproval(request)),
        StreamedTurnResult::Cancelled => Err(MoaError::ProviderError(
            "buffered brain turn was cancelled unexpectedly".to_string(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_streamed_turn_with_tools_mode(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    mut signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    turn_requested: Option<&mut bool>,
    queued_messages: Option<&mut Vec<UserMessage>>,
    soft_cancel_requested: Option<&mut bool>,
    tool_loop_mode: ToolLoopMode,
) -> Result<StreamedTurnResult> {
    let mut local_turn_requested = false;
    let turn_requested = turn_requested.unwrap_or(&mut local_turn_requested);
    let mut local_queued_messages = Vec::new();
    let queued_messages = queued_messages.unwrap_or(&mut local_queued_messages);
    let mut local_soft_cancel_requested = false;
    let soft_cancel_requested = soft_cancel_requested.unwrap_or(&mut local_soft_cancel_requested);

    loop {
        let session = session_store.get_session(session_id.clone()).await?;
        let events = session_store
            .get_events(session_id.clone(), EventRange::all())
            .await?;

        if let Some(router) = &tool_router {
            if process_resolved_approval(
                session_id.clone(),
                &session,
                session_store.clone(),
                router,
                event_tx,
                runtime_tx,
                &events,
                cancel_token,
                hard_cancel_token,
            )
            .await?
            {
                if *soft_cancel_requested {
                    return Ok(StreamedTurnResult::Cancelled);
                }
                if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                    return Ok(StreamedTurnResult::Continue);
                }
                continue;
            }

            if let Some(pending) = find_pending_tool_approval(&events) {
                if let Some(receiver) = signal_rx.as_deref_mut() {
                    let outcome = wait_for_approval(
                        session_id.clone(),
                        &session,
                        session_store.clone(),
                        router,
                        pending,
                        event_tx,
                        runtime_tx,
                        cancel_token,
                        hard_cancel_token,
                        receiver,
                        turn_requested,
                        queued_messages,
                        soft_cancel_requested,
                    )
                    .await?;
                    match outcome {
                        ToolCallOutcome::Executed | ToolCallOutcome::Skipped => {
                            if *soft_cancel_requested {
                                return Ok(StreamedTurnResult::Cancelled);
                            }
                            if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                                return Ok(StreamedTurnResult::Continue);
                            }
                            continue;
                        }
                        ToolCallOutcome::NeedsApproval(request) => {
                            return Ok(StreamedTurnResult::NeedsApproval(request));
                        }
                        ToolCallOutcome::Cancelled => {
                            return Ok(StreamedTurnResult::Cancelled);
                        }
                    }
                } else if let Some(request) = find_pending_approval_request(&events) {
                    session_store
                        .update_status(session_id.clone(), SessionStatus::WaitingApproval)
                        .await?;
                    return Ok(StreamedTurnResult::NeedsApproval(request));
                }
            }
        }

        let mut ctx = WorkingContext::new(&session, llm_provider.capabilities());

        let stage_reports = pipeline.run(&mut ctx).await?;
        let active_canary = tool_router.as_ref().map(|_| inject_canary(&mut ctx));
        tracing::info!(
            session_id = %session_id,
            compiled_messages = ctx.messages.len(),
            total_tokens = ctx.token_count,
            stages = stage_reports.len(),
            "compiled context for streamed brain turn"
        );

        let mut emit_runtime = |event| {
            let _ = runtime_tx.send(event);
        };

        let streamed = if let Some(receiver) = signal_rx.as_deref_mut() {
            stream_completion_response(
                llm_provider.clone(),
                ctx.into_request(),
                cancel_token,
                Some(receiver),
                &mut emit_runtime,
                |signal| {
                    handle_stream_signal(
                        signal,
                        runtime_tx,
                        turn_requested,
                        queued_messages,
                        soft_cancel_requested,
                    )
                },
            )
            .await?
        } else {
            stream_completion_response(
                llm_provider.clone(),
                ctx.into_request(),
                cancel_token,
                None,
                &mut emit_runtime,
                |_| StreamSignalDisposition::Continue,
            )
            .await?
        };
        if streamed.cancelled {
            return Ok(StreamedTurnResult::Cancelled);
        }
        let response = streamed.response.ok_or_else(|| {
            MoaError::ProviderError(
                "streamed turn finished without a provider response".to_string(),
            )
        })?;

        if !streamed.streamed_text.trim().is_empty() {
            append_event(
                &session_store,
                event_tx,
                session_id.clone(),
                Event::BrainResponse {
                    text: streamed.streamed_text.clone(),
                    model: response.model.clone(),
                    input_tokens: response.input_tokens,
                    output_tokens: response.output_tokens,
                    cost_cents: 0,
                    duration_ms: response.duration_ms,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::AssistantFinished {
                text: streamed.streamed_text,
            });
        }

        let mut emitted_tool_calls = 0usize;
        let mut saw_tool_request = false;
        let mut executed_tools = false;
        for block in &response.content {
            if let CompletionContent::ToolCall(call) = block {
                saw_tool_request = true;
                let outcome = handle_tool_call(
                    session_id.clone(),
                    &session,
                    session_store.clone(),
                    tool_router.as_deref(),
                    call,
                    active_canary.as_deref(),
                    event_tx,
                    runtime_tx,
                    cancel_token,
                    hard_cancel_token,
                    signal_rx.as_deref_mut(),
                    turn_requested,
                    queued_messages,
                    soft_cancel_requested,
                )
                .await?;
                emitted_tool_calls += 1;
                match outcome {
                    ToolCallOutcome::Executed => executed_tools = true,
                    ToolCallOutcome::Skipped => {}
                    ToolCallOutcome::NeedsApproval(request) => {
                        return Ok(StreamedTurnResult::NeedsApproval(request));
                    }
                    ToolCallOutcome::Cancelled => {
                        return Ok(StreamedTurnResult::Cancelled);
                    }
                }
                if signal_rx.is_some() {
                    drain_signal_queue(
                        signal_rx.as_deref_mut(),
                        runtime_tx,
                        turn_requested,
                        queued_messages,
                        soft_cancel_requested,
                    )?;
                }
                if *soft_cancel_requested {
                    return Ok(StreamedTurnResult::Cancelled);
                }
            }
        }

        let updated_session = session_store.get_session(session_id.clone()).await?;
        let _ = runtime_tx.send(RuntimeEvent::UsageUpdated {
            total_tokens: updated_session.total_input_tokens + updated_session.total_output_tokens,
        });

        tracing::info!(
            session_id = %session_id,
            tool_calls = emitted_tool_calls,
            stop_reason = ?response.stop_reason,
            "streamed brain turn completed"
        );

        if *soft_cancel_requested {
            return Ok(StreamedTurnResult::Cancelled);
        }

        if executed_tools || saw_tool_request || response.stop_reason == StopReason::ToolUse {
            if tool_router.is_some() {
                if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                    return Ok(StreamedTurnResult::Continue);
                }
                continue;
            }
            return Ok(StreamedTurnResult::Continue);
        }

        if response.stop_reason == StopReason::EndTurn {
            return Ok(StreamedTurnResult::Complete);
        }

        return Ok(StreamedTurnResult::Continue);
    }
}

enum ToolCallOutcome {
    Executed,
    Skipped,
    NeedsApproval(ApprovalRequest),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn handle_tool_call(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: Option<&ToolRouter>,
    call: &ToolInvocation,
    active_canary: Option<&str>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<ToolCallOutcome> {
    let tool_id = parse_tool_id(call);
    let serialized_input = serde_json::to_string(&call.input)?;

    if contains_canary_tokens(&serialized_input)
        || active_canary
            .map(|canary| check_canary(canary, &serialized_input))
            .unwrap_or(false)
    {
        append_event(
            &session_store,
            event_tx,
            session_id.clone(),
            Event::Warning {
                message: format!(
                    "blocked tool {} because the active canary leaked into tool input",
                    call.name
                ),
            },
        )
        .await?;
        append_event(
            &session_store,
            event_tx,
            session_id,
            Event::ToolError {
                tool_id,
                error: format!(
                    "tool {} blocked because it leaked a protected canary token",
                    call.name
                ),
                retryable: false,
            },
        )
        .await?;
        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
            tool_id,
            tool_name: call.name.clone(),
            status: ToolCardStatus::Failed,
            summary: format!("{} blocked", call.name),
            detail: Some(
                "Blocked because a protected canary token leaked into tool input".to_string(),
            ),
        }));
        return Ok(ToolCallOutcome::Skipped);
    }

    let Some(router) = tool_router else {
        append_event(
            &session_store,
            event_tx,
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: call.id.clone(),
                tool_name: call.name.clone(),
                input: call.input.clone(),
                hand_id: None,
            },
        )
        .await?;
        return Ok(ToolCallOutcome::Skipped);
    };

    let prepared = router.prepare_invocation(session, call).await?;
    let summary = prepared.policy_input.input_summary.clone();

    match prepared.policy.action {
        PolicyAction::Allow => {
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Running,
                summary,
                detail: None,
            }));
            execute_tool(
                session_id,
                session,
                session_store,
                router,
                call,
                tool_id,
                true,
                active_canary,
                event_tx,
                runtime_tx,
                cancel_token,
                hard_cancel_token,
            )
            .await
        }
        PolicyAction::Deny => {
            append_event(
                &session_store,
                event_tx,
                session_id.clone(),
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    hand_id: None,
                },
            )
            .await?;
            let message = format!("tool {} denied by policy", call.name);
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::ToolError {
                    tool_id,
                    error: message.clone(),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary,
                detail: Some(message),
            }));
            Ok(ToolCallOutcome::Skipped)
        }
        PolicyAction::RequireApproval => {
            append_event(
                &session_store,
                event_tx,
                session_id.clone(),
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    hand_id: None,
                },
            )
            .await?;
            let request = ApprovalRequest {
                request_id: tool_id,
                tool_name: call.name.clone(),
                input_summary: summary.clone(),
                risk_level: prepared.policy_input.risk_level.clone(),
            };
            let prompt = ApprovalPrompt {
                request: request.clone(),
                pattern: prepared.always_allow_pattern,
                parameters: prepared.approval_fields,
                file_diffs: prepared.approval_diffs,
            };
            append_event(
                &session_store,
                event_tx,
                session_id.clone(),
                Event::ApprovalRequested {
                    request_id: request.request_id,
                    tool_name: request.tool_name.clone(),
                    input_summary: request.input_summary.clone(),
                    risk_level: request.risk_level.clone(),
                    prompt: Some(prompt.clone()),
                },
            )
            .await?;
            session_store
                .update_status(session_id.clone(), SessionStatus::WaitingApproval)
                .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::WaitingApproval,
                summary: summary.clone(),
                detail: Some("Press y to allow once, a to always allow, n to deny".to_string()),
            }));
            let _ = runtime_tx.send(RuntimeEvent::ApprovalRequested(prompt));

            if let Some(receiver) = signal_rx {
                wait_for_signal_approval(
                    session_id,
                    session,
                    session_store,
                    router,
                    call,
                    tool_id,
                    summary,
                    active_canary,
                    event_tx,
                    runtime_tx,
                    cancel_token,
                    hard_cancel_token,
                    receiver,
                    turn_requested,
                    queued_messages,
                    soft_cancel_requested,
                )
                .await
            } else {
                Ok(ToolCallOutcome::NeedsApproval(request))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_signal_approval(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    call: &ToolInvocation,
    tool_id: Uuid,
    summary: String,
    active_canary: Option<&str>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<ToolCallOutcome> {
    loop {
        match signal_rx.recv().await {
            Some(SessionSignal::ApprovalDecided {
                request_id,
                decision,
            }) if request_id == tool_id => {
                append_event(
                    &session_store,
                    event_tx,
                    session_id.clone(),
                    Event::ApprovalDecided {
                        request_id,
                        decision: decision.clone(),
                        decided_by: session.user_id.to_string(),
                        decided_at: chrono::Utc::now(),
                    },
                )
                .await?;
                session_store
                    .update_status(session_id.clone(), SessionStatus::Running)
                    .await?;

                return match decision {
                    ApprovalDecision::AllowOnce => {
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Running,
                            summary,
                            detail: None,
                        }));
                        execute_tool(
                            session_id,
                            session,
                            session_store,
                            tool_router,
                            call,
                            tool_id,
                            false,
                            active_canary,
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
                        )
                        .await
                    }
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        tool_router
                            .store_approval_rule(
                                session,
                                &call.name,
                                &pattern,
                                PolicyAction::Allow,
                                session.user_id.clone(),
                            )
                            .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Running,
                            summary,
                            detail: Some(format!("Always allow rule stored: {pattern}")),
                        }));
                        execute_tool(
                            session_id,
                            session,
                            session_store,
                            tool_router,
                            call,
                            tool_id,
                            false,
                            active_canary,
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
                        )
                        .await
                    }
                    ApprovalDecision::Deny { reason } => {
                        append_event(
                            &session_store,
                            event_tx,
                            session_id,
                            Event::ToolError {
                                tool_id,
                                error: reason
                                    .clone()
                                    .unwrap_or_else(|| "tool execution denied by user".to_string()),
                                retryable: false,
                            },
                        )
                        .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Failed,
                            summary,
                            detail: Some(
                                reason.unwrap_or_else(|| "Denied by the user".to_string()),
                            ),
                        }));
                        Ok(ToolCallOutcome::Skipped)
                    }
                };
            }
            Some(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after the approval decision.".to_string(),
                ));
            }
            Some(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Stop requested. MOA will stop after the current step.".to_string(),
                ));
                return Ok(ToolCallOutcome::Cancelled);
            }
            Some(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
                return Ok(ToolCallOutcome::Cancelled);
            }
            Some(SessionSignal::ApprovalDecided { .. }) => {}
            None => {
                return Err(MoaError::ProviderError(
                    "approval channel closed".to_string(),
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_resolved_approval(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    events: &[EventRecord],
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<bool> {
    let Some(pending) = find_resolved_pending_tool_approval(events) else {
        return Ok(false);
    };

    match pending.decision.clone() {
        StoredApprovalDecision::AllowOnce => {
            let invocation = ToolInvocation {
                id: Some(pending.tool_id.to_string()),
                name: pending.tool_name.clone(),
                input: pending.input.clone(),
            };
            let prepared = tool_router.prepare_invocation(session, &invocation).await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id,
                tool_name: pending.tool_name.clone(),
                status: ToolCardStatus::Running,
                summary: prepared.policy_input.input_summary,
                detail: None,
            }));
            execute_pending_tool(
                session_id,
                session,
                session_store,
                tool_router,
                event_tx,
                runtime_tx,
                pending,
                None,
                cancel_token,
                hard_cancel_token,
            )
            .await?;
        }
        StoredApprovalDecision::AlwaysAllow {
            pattern,
            decided_by,
        } => {
            tool_router
                .store_approval_rule(
                    session,
                    &pending.tool_name,
                    &pattern,
                    PolicyAction::Allow,
                    UserId::new(decided_by.clone()),
                )
                .await?;
            let invocation = ToolInvocation {
                id: Some(pending.tool_id.to_string()),
                name: pending.tool_name.clone(),
                input: pending.input.clone(),
            };
            let prepared = tool_router.prepare_invocation(session, &invocation).await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id,
                tool_name: pending.tool_name.clone(),
                status: ToolCardStatus::Running,
                summary: prepared.policy_input.input_summary,
                detail: Some(format!("Always allow rule stored: {pattern}")),
            }));
            execute_pending_tool(
                session_id,
                session,
                session_store,
                tool_router,
                event_tx,
                runtime_tx,
                pending,
                None,
                cancel_token,
                hard_cancel_token,
            )
            .await?;
        }
        StoredApprovalDecision::Deny { reason } => {
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::ToolError {
                    tool_id: pending.tool_id,
                    error: reason
                        .clone()
                        .unwrap_or_else(|| "tool execution denied by user".to_string()),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id,
                tool_name: pending.tool_name,
                status: ToolCardStatus::Failed,
                summary: "tool denied".to_string(),
                detail: reason,
            }));
        }
    }

    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_approval(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    pending: PendingToolApproval,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<ToolCallOutcome> {
    let invocation = ToolInvocation {
        id: Some(pending.tool_id.to_string()),
        name: pending.tool_name.clone(),
        input: pending.input.clone(),
    };
    let prepared = tool_router.prepare_invocation(session, &invocation).await?;
    let prompt = ApprovalPrompt {
        request: ApprovalRequest {
            request_id: pending.tool_id,
            tool_name: pending.tool_name.clone(),
            input_summary: prepared.policy_input.input_summary.clone(),
            risk_level: prepared.policy_input.risk_level.clone(),
        },
        pattern: prepared.always_allow_pattern.clone(),
        parameters: prepared.approval_fields.clone(),
        file_diffs: prepared.approval_diffs.clone(),
    };
    let _ = runtime_tx.send(RuntimeEvent::ApprovalRequested(prompt));
    let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
        tool_id: pending.tool_id,
        tool_name: pending.tool_name.clone(),
        status: ToolCardStatus::WaitingApproval,
        summary: prepared.policy_input.input_summary.clone(),
        detail: Some("Press y to allow once, a to always allow, n to deny".to_string()),
    }));

    loop {
        match signal_rx.recv().await {
            Some(SessionSignal::ApprovalDecided {
                request_id,
                decision,
            }) if request_id == pending.tool_id => {
                append_event(
                    &session_store,
                    event_tx,
                    session_id.clone(),
                    Event::ApprovalDecided {
                        request_id,
                        decision: decision.clone(),
                        decided_by: session.user_id.to_string(),
                        decided_at: chrono::Utc::now(),
                    },
                )
                .await?;
                return match decision {
                    ApprovalDecision::AllowOnce => {
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id: pending.tool_id,
                            tool_name: pending.tool_name.clone(),
                            status: ToolCardStatus::Running,
                            summary: prepared.policy_input.input_summary.clone(),
                            detail: None,
                        }));
                        execute_tool(
                            session_id,
                            session,
                            session_store,
                            tool_router,
                            &invocation,
                            pending.tool_id,
                            false,
                            None,
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
                        )
                        .await
                    }
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        tool_router
                            .store_approval_rule(
                                session,
                                &pending.tool_name,
                                &pattern,
                                PolicyAction::Allow,
                                session.user_id.clone(),
                            )
                            .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id: pending.tool_id,
                            tool_name: pending.tool_name.clone(),
                            status: ToolCardStatus::Running,
                            summary: prepared.policy_input.input_summary.clone(),
                            detail: Some(format!("Always allow rule stored: {pattern}")),
                        }));
                        execute_tool(
                            session_id,
                            session,
                            session_store,
                            tool_router,
                            &invocation,
                            pending.tool_id,
                            false,
                            None,
                            event_tx,
                            runtime_tx,
                            cancel_token,
                            hard_cancel_token,
                        )
                        .await
                    }
                    ApprovalDecision::Deny { reason } => {
                        append_event(
                            &session_store,
                            event_tx,
                            session_id,
                            Event::ToolError {
                                tool_id: pending.tool_id,
                                error: reason
                                    .clone()
                                    .unwrap_or_else(|| "tool execution denied by user".to_string()),
                                retryable: false,
                            },
                        )
                        .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id: pending.tool_id,
                            tool_name: pending.tool_name,
                            status: ToolCardStatus::Failed,
                            summary: "tool denied".to_string(),
                            detail: reason,
                        }));
                        Ok(ToolCallOutcome::Skipped)
                    }
                };
            }
            Some(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after the approval decision.".to_string(),
                ));
            }
            Some(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Stop requested. MOA will stop after the current step.".to_string(),
                ));
                return Ok(ToolCallOutcome::Cancelled);
            }
            Some(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
                return Ok(ToolCallOutcome::Cancelled);
            }
            Some(SessionSignal::ApprovalDecided { .. }) => {}
            None => {
                return Err(MoaError::ProviderError(
                    "approval channel closed".to_string(),
                ));
            }
        }
    }
}

fn drain_signal_queue(
    signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<()> {
    let Some(signal_rx) = signal_rx else {
        return Ok(());
    };

    loop {
        match signal_rx.try_recv() {
            Ok(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after current turn.".to_string(),
                ));
            }
            Ok(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::ApprovalDecided { .. }) => {}
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err(MoaError::ProviderError(
                    "session signal channel closed".to_string(),
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_tool(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    call: &ToolInvocation,
    tool_id: Uuid,
    emit_call_event: bool,
    active_canary: Option<&str>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolCallOutcome> {
    match tool_router
        .execute_authorized_with_cancel(session, call, cancel_token, hard_cancel_token)
        .await
    {
        Ok((resolved_hand_id, output)) => {
            if emit_call_event {
                append_event(
                    &session_store,
                    event_tx,
                    session_id.clone(),
                    Event::ToolCall {
                        tool_id,
                        provider_tool_use_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        input: call.input.clone(),
                        hand_id: resolved_hand_id,
                    },
                )
                .await?;
            }
            let secured_output = secure_tool_output(&output, active_canary);
            emit_tool_output_warning(
                session_id.clone(),
                &session_store,
                event_tx,
                tool_id,
                &call.name,
                &secured_output,
            )
            .await?;
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: call.id.clone(),
                    output: output.clone(),
                    success: !output.is_error,
                    duration_ms: output.duration.as_millis() as u64,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: if output.is_error {
                    ToolCardStatus::Failed
                } else {
                    ToolCardStatus::Succeeded
                },
                summary: summarize_tool_completion(call, &output),
                detail: Some(output.to_text()),
            }));
            Ok(ToolCallOutcome::Executed)
        }
        Err(MoaError::Cancelled) => {
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::ToolError {
                    tool_id,
                    error: "cancelled".to_string(),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary: format!("{} cancelled", call.name),
                detail: Some("cancelled".to_string()),
            }));
            Ok(ToolCallOutcome::Cancelled)
        }
        Err(error) => {
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::ToolError {
                    tool_id,
                    error: error.to_string(),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary: format!("{} failed", call.name),
                detail: Some(error.to_string()),
            }));
            Ok(ToolCallOutcome::Skipped)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_pending_tool(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    pending: PendingToolApproval,
    active_canary: Option<&str>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<()> {
    let invocation = ToolInvocation {
        id: Some(pending.tool_id.to_string()),
        name: pending.tool_name.clone(),
        input: pending.input.clone(),
    };
    let _ = execute_tool(
        session_id,
        session,
        session_store,
        tool_router,
        &invocation,
        pending.tool_id,
        false,
        active_canary,
        event_tx,
        runtime_tx,
        cancel_token,
        hard_cancel_token,
    )
    .await?;
    Ok(())
}

fn buffer_queued_message(queued_messages: &mut Vec<UserMessage>, message: UserMessage) {
    queued_messages.push(message);
}

fn handle_stream_signal(
    signal: SessionSignal,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> StreamSignalDisposition {
    match signal {
        SessionSignal::QueueMessage(message) => {
            buffer_queued_message(queued_messages, message);
            *turn_requested = true;
            let _ = runtime_tx.send(RuntimeEvent::Notice(
                "Message queued. Will process after current turn.".to_string(),
            ));
            StreamSignalDisposition::Continue
        }
        SessionSignal::SoftCancel => {
            *soft_cancel_requested = true;
            let _ = runtime_tx.send(RuntimeEvent::Notice(
                "Stop requested. MOA will stop after the current step.".to_string(),
            ));
            StreamSignalDisposition::Continue
        }
        SessionSignal::HardCancel => StreamSignalDisposition::CancelImmediately,
        SessionSignal::ApprovalDecided { .. } => StreamSignalDisposition::Continue,
    }
}

async fn append_event(
    session_store: &Arc<dyn SessionStore>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    session_id: SessionId,
    event: Event,
) -> Result<()> {
    let sequence_num = session_store.emit_event(session_id.clone(), event).await?;
    if let Some(event_tx) = event_tx {
        let mut records = session_store
            .get_events(
                session_id,
                EventRange {
                    from_seq: Some(sequence_num),
                    to_seq: Some(sequence_num),
                    event_types: None,
                    limit: Some(1),
                },
            )
            .await?;
        let record = records
            .pop()
            .ok_or_else(|| MoaError::StorageError("failed to reload appended event".to_string()))?;
        let _ = event_tx.send(record);
    }
    Ok(())
}

fn format_tool_output(output: &moa_core::ToolOutput) -> String {
    output.to_text()
}

fn summarize_tool_completion(call: &ToolInvocation, output: &moa_core::ToolOutput) -> String {
    if !output.is_error {
        format!(
            "{} completed in {} ms",
            call.name,
            output.duration.as_millis()
        )
    } else {
        match output.process_exit_code() {
            Some(exit_code) => format!("{} exited with code {}", call.name, exit_code),
            None => format!("{} failed", call.name),
        }
    }
}

fn parse_tool_id(call: &ToolInvocation) -> Uuid {
    call.id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4)
}

struct SecuredToolOutput {
    inspection: moa_security::InputInspection,
}

fn secure_tool_output(
    output: &moa_core::ToolOutput,
    active_canary: Option<&str>,
) -> SecuredToolOutput {
    let formatted = format_tool_output(output);
    let canaries = active_canary
        .map(|canary| vec![canary.to_string()])
        .unwrap_or_default();
    let inspection = inspect_input(&formatted, &canaries);
    SecuredToolOutput { inspection }
}

async fn emit_tool_output_warning(
    session_id: SessionId,
    session_store: &Arc<dyn SessionStore>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    tool_id: Uuid,
    tool_name: &str,
    secured_output: &SecuredToolOutput,
) -> Result<()> {
    if matches!(
        secured_output.inspection.classification,
        InputClassification::MediumRisk | InputClassification::HighRisk
    ) {
        append_event(
            session_store,
            event_tx,
            session_id,
            Event::Warning {
                message: format!(
                    "tool output for {tool_name} ({tool_id}) classified as {:?} with signals: {}",
                    secured_output.inspection.classification,
                    secured_output.inspection.signals.join(", ")
                ),
            },
        )
        .await?;
    }

    Ok(())
}
