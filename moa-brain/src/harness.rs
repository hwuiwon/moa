//! Single-turn brain harness execution.

use std::sync::Arc;

use moa_core::{
    ApprovalRequest, CompletionContent, Event, EventRange, LLMProvider, PolicyAction, Result,
    SessionId, SessionStore, StopReason, WorkingContext,
};
use moa_hands::ToolRouter;
use moa_security::{
    InputClassification, check_canary, contains_canary_tokens, inject_canary, inspect_input,
};
use uuid::Uuid;

use crate::pipeline::ContextPipeline;
use crate::turn::{
    PendingToolApproval, StoredApprovalDecision, StreamSignalDisposition,
    find_pending_approval_request, find_resolved_pending_tool_approval, stream_completion_response,
};

/// Outcome of a single brain turn.
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

/// Runs one turn of the brain harness.
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

/// Runs one turn of the brain harness with optional tool execution support.
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

/// Runs one turn of the brain harness, yielding after any tool execution boundary.
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

async fn run_brain_turn_with_tools_mode(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
    tool_loop_mode: ToolLoopMode,
) -> Result<TurnResult> {
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
                &events,
            )
            .await?
            {
                session_store
                    .update_status(session_id.clone(), moa_core::SessionStatus::Running)
                    .await?;
                if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                    return Ok(TurnResult::Continue);
                }
                continue;
            }

            if let Some(request) = find_pending_approval_request(&events) {
                session_store
                    .update_status(session_id.clone(), moa_core::SessionStatus::WaitingApproval)
                    .await?;
                return Ok(TurnResult::NeedsApproval(request));
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
            "compiled context for brain turn"
        );

        let streamed = stream_completion_response(
            llm_provider.clone(),
            ctx.into_request(),
            None,
            |_| {},
            |_| StreamSignalDisposition::Continue,
        )
        .await?;
        let response = streamed.response.ok_or_else(|| {
            moa_core::MoaError::ProviderError(
                "buffered brain turn was cancelled unexpectedly".to_string(),
            )
        })?;
        let mut emitted_tool_calls = 0usize;

        if !response.text.trim().is_empty() {
            session_store
                .emit_event(
                    session_id.clone(),
                    Event::BrainResponse {
                        text: response.text.clone(),
                        model: response.model.clone(),
                        input_tokens: response.input_tokens,
                        output_tokens: response.output_tokens,
                        cost_cents: 0,
                        duration_ms: response.duration_ms,
                    },
                )
                .await?;
        }

        let mut executed_tools = false;
        for block in &response.content {
            if let CompletionContent::ToolCall(call) = block {
                let tool_id = call
                    .id
                    .as_deref()
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .unwrap_or_else(Uuid::new_v4);
                let serialized_input = serde_json::to_string(&call.input)?;

                if contains_canary_tokens(&serialized_input)
                    || active_canary
                        .as_deref()
                        .map(|canary| check_canary(canary, &serialized_input))
                        .unwrap_or(false)
                {
                    session_store
                        .emit_event(
                            session_id.clone(),
                            Event::Warning {
                                message: format!(
                                    "blocked tool {} because the active canary leaked into tool input",
                                    call.name
                                ),
                            },
                        )
                        .await?;
                    session_store
                        .emit_event(
                            session_id.clone(),
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
                    continue;
                }

                if let Some(router) = &tool_router {
                    let prepared = router.prepare_invocation(&session, call).await?;
                    match prepared.policy.action {
                        PolicyAction::Allow => {
                            let (resolved_hand_id, output) =
                                router.execute_authorized(&session, call).await?;
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolCall {
                                        tool_id,
                                        tool_name: call.name.clone(),
                                        input: call.input.clone(),
                                        hand_id: resolved_hand_id,
                                    },
                                )
                                .await?;
                            let secured_output =
                                secure_tool_output(&output, active_canary.as_deref());
                            emit_tool_output_warning(
                                session_id.clone(),
                                &session_store,
                                tool_id,
                                &call.name,
                                &secured_output,
                            )
                            .await?;
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolResult {
                                        tool_id,
                                        output: output.clone(),
                                        success: !output.is_error,
                                        duration_ms: output.duration.as_millis() as u64,
                                    },
                                )
                                .await?;
                            executed_tools = true;
                        }
                        PolicyAction::Deny => {
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolCall {
                                        tool_id,
                                        tool_name: call.name.clone(),
                                        input: call.input.clone(),
                                        hand_id: None,
                                    },
                                )
                                .await?;
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolError {
                                        tool_id,
                                        error: format!("tool {} denied by policy", call.name),
                                        retryable: false,
                                    },
                                )
                                .await?;
                        }
                        PolicyAction::RequireApproval => {
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolCall {
                                        tool_id,
                                        tool_name: call.name.clone(),
                                        input: call.input.clone(),
                                        hand_id: None,
                                    },
                                )
                                .await?;
                            let request = ApprovalRequest {
                                request_id: tool_id,
                                tool_name: call.name.clone(),
                                input_summary: prepared.policy_input.input_summary,
                                risk_level: prepared.policy_input.risk_level,
                            };
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ApprovalRequested {
                                        request_id: request.request_id,
                                        tool_name: request.tool_name.clone(),
                                        input_summary: request.input_summary.clone(),
                                        risk_level: request.risk_level.clone(),
                                    },
                                )
                                .await?;
                            session_store
                                .update_status(
                                    session_id.clone(),
                                    moa_core::SessionStatus::WaitingApproval,
                                )
                                .await?;
                            return Ok(TurnResult::NeedsApproval(request));
                        }
                    }
                } else {
                    session_store
                        .emit_event(
                            session_id.clone(),
                            Event::ToolCall {
                                tool_id,
                                tool_name: call.name.clone(),
                                input: call.input.clone(),
                                hand_id: None,
                            },
                        )
                        .await?;
                }

                emitted_tool_calls += 1;
            }
        }

        tracing::info!(
            session_id = %session_id,
            tool_calls = emitted_tool_calls,
            stop_reason = ?response.stop_reason,
            "brain turn completed"
        );

        if executed_tools || response.stop_reason == StopReason::ToolUse {
            if tool_router.is_some() {
                if matches!(tool_loop_mode, ToolLoopMode::StepAfterToolBoundary) {
                    return Ok(TurnResult::Continue);
                }
                continue;
            }
            return Ok(TurnResult::Continue);
        }

        if response.stop_reason == StopReason::EndTurn {
            return Ok(TurnResult::Complete);
        }

        return Ok(TurnResult::Continue);
    }
}

fn format_tool_output(output: &moa_core::ToolOutput) -> String {
    output.to_text()
}

async fn process_resolved_approval(
    session_id: SessionId,
    session: &moa_core::SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    events: &[moa_core::EventRecord],
) -> Result<bool> {
    let Some(pending) = find_resolved_pending_tool_approval(events) else {
        return Ok(false);
    };

    match pending.decision.clone() {
        StoredApprovalDecision::AllowOnce => {
            execute_pending_tool(session_id, session, session_store, tool_router, pending).await?;
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
                    moa_core::UserId::new(decided_by.clone()),
                )
                .await?;
            execute_pending_tool(session_id, session, session_store, tool_router, pending).await?;
        }
        StoredApprovalDecision::Deny { reason } => {
            session_store
                .emit_event(
                    session_id,
                    Event::ToolError {
                        tool_id: pending.tool_id,
                        error: reason
                            .unwrap_or_else(|| "tool execution denied by user".to_string()),
                        retryable: false,
                    },
                )
                .await?;
        }
    }

    Ok(true)
}

async fn execute_pending_tool(
    session_id: SessionId,
    session: &moa_core::SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    pending: PendingToolApproval,
) -> Result<()> {
    let invocation = moa_core::ToolInvocation {
        id: Some(pending.tool_id.to_string()),
        name: pending.tool_name.clone(),
        input: pending.input.clone(),
    };
    match tool_router.execute_authorized(session, &invocation).await {
        Ok((resolved_hand_id, output)) => {
            if let Some(hand_id) = resolved_hand_id {
                tracing::debug!(session_id = %session_id, hand_id, "executed approved tool call");
            }
            let secured_output = secure_tool_output(&output, None);
            emit_tool_output_warning(
                session_id.clone(),
                &session_store,
                pending.tool_id,
                &pending.tool_name,
                &secured_output,
            )
            .await?;
            session_store
                .emit_event(
                    session_id,
                    Event::ToolResult {
                        tool_id: pending.tool_id,
                        output: output.clone(),
                        success: !output.is_error,
                        duration_ms: output.duration.as_millis() as u64,
                    },
                )
                .await?;
        }
        Err(error) => {
            session_store
                .emit_event(
                    session_id,
                    Event::ToolError {
                        tool_id: pending.tool_id,
                        error: error.to_string(),
                        retryable: false,
                    },
                )
                .await?;
        }
    }

    Ok(())
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
    tool_id: Uuid,
    tool_name: &str,
    secured_output: &SecuredToolOutput,
) -> Result<()> {
    if matches!(
        secured_output.inspection.classification,
        InputClassification::MediumRisk | InputClassification::HighRisk
    ) {
        session_store
            .emit_event(
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
