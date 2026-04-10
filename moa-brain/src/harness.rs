//! Single-turn brain harness execution.

use std::sync::Arc;

use moa_core::{
    ApprovalRequest, CompletionContent, Event, EventRange, LLMProvider, PolicyAction, Result,
    SessionId, SessionStore, StopReason, WorkingContext,
};
use moa_hands::ToolRouter;
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

/// Runs one turn of the brain harness.
pub async fn run_brain_turn(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
) -> Result<TurnResult> {
    run_brain_turn_with_tools(session_id, session_store, llm_provider, pipeline, None).await
}

/// Runs one turn of the brain harness with optional tool execution support.
pub async fn run_brain_turn_with_tools(
    session_id: SessionId,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LLMProvider>,
    pipeline: &ContextPipeline,
    tool_router: Option<Arc<ToolRouter>>,
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
                            session_store
                                .emit_event(
                                    session_id.clone(),
                                    Event::ToolResult {
                                        tool_id,
                                        output: format_tool_output(&output),
                                        success: output.exit_code == 0,
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
    let mut sections = Vec::new();
    if !output.stdout.trim().is_empty() {
        sections.push(output.stdout.trim_end().to_string());
    }
    if !output.stderr.trim().is_empty() {
        sections.push(format!("stderr:\n{}", output.stderr.trim_end()));
    }
    if sections.is_empty() {
        format!("exit_code: {}", output.exit_code)
    } else {
        sections.join("\n\n")
    }
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
            session_store
                .emit_event(
                    session_id,
                    Event::ToolResult {
                        tool_id: pending.tool_id,
                        output: format_tool_output(&output),
                        success: output.exit_code == 0,
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
