//! Single-turn brain harness execution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, ApprovalRequest, CompletionContent, Event, EventRange, LLMProvider,
    PolicyAction, Result, SessionId, SessionStore, StopReason, WorkingContext,
};
use moa_hands::ToolRouter;
use uuid::Uuid;

use crate::pipeline::ContextPipeline;

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

            if let Some(request) = find_pending_approval(&events)? {
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

        let response = llm_provider
            .complete(ctx.into_request())
            .await?
            .collect()
            .await?;
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
                    let policy = router.check_policy(&session, call).await?;
                    match policy.action {
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
                                input_summary: policy.input_summary,
                                risk_level: policy.risk_level,
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
    let Some(pending) = find_resolved_pending_tool(events)? else {
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

fn find_pending_approval(events: &[moa_core::EventRecord]) -> Result<Option<ApprovalRequest>> {
    let mut requests = Vec::new();
    let mut decisions = HashSet::new();
    let mut completed = HashSet::new();

    for record in events {
        match &record.event {
            Event::ApprovalRequested {
                request_id,
                tool_name,
                input_summary,
                risk_level,
            } => {
                requests.push((
                    record.sequence_num,
                    ApprovalRequest {
                        request_id: *request_id,
                        tool_name: tool_name.clone(),
                        input_summary: input_summary.clone(),
                        risk_level: risk_level.clone(),
                    },
                ));
            }
            Event::ApprovalDecided { request_id, .. } => {
                decisions.insert(*request_id);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(*tool_id);
            }
            _ => {}
        }
    }

    requests.sort_by_key(|(sequence_num, _)| *sequence_num);
    for (_, request) in requests {
        if !decisions.contains(&request.request_id) && !completed.contains(&request.request_id) {
            return Ok(Some(request));
        }
    }

    Ok(None)
}

fn find_resolved_pending_tool(
    events: &[moa_core::EventRecord],
) -> Result<Option<PendingToolApproval>> {
    let mut tool_calls = HashMap::new();
    let mut decisions = HashMap::new();
    let mut completed = HashSet::new();
    let mut requested = HashSet::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        decision: StoredApprovalDecision::AllowOnce,
                        sequence_num: record.sequence_num,
                    },
                );
            }
            Event::ApprovalRequested { request_id, .. } => {
                requested.insert(*request_id);
            }
            Event::ApprovalDecided {
                request_id,
                decision,
                decided_by,
                ..
            } => {
                let stored = match decision {
                    ApprovalDecision::AllowOnce => StoredApprovalDecision::AllowOnce,
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        StoredApprovalDecision::AlwaysAllow {
                            pattern: pattern.clone(),
                            decided_by: decided_by.clone(),
                        }
                    }
                    ApprovalDecision::Deny { reason } => StoredApprovalDecision::Deny {
                        reason: reason.clone(),
                    },
                };
                decisions.insert(*request_id, stored);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(*tool_id);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter_map(|mut pending| {
            if completed.contains(&pending.tool_id) || !requested.contains(&pending.tool_id) {
                return None;
            }
            let decision = decisions.get(&pending.tool_id)?.clone();
            pending.decision = decision;
            Some(pending)
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence_num);

    Ok(pending.into_iter().next())
}

#[derive(Debug, Clone)]
struct PendingToolApproval {
    tool_id: Uuid,
    tool_name: String,
    input: serde_json::Value,
    decision: StoredApprovalDecision,
    sequence_num: u64,
}

#[derive(Debug, Clone)]
enum StoredApprovalDecision {
    AllowOnce,
    AlwaysAllow { pattern: String, decided_by: String },
    Deny { reason: Option<String> },
}
