//! Tool call dispatch, execution, and output handling for the harness.

use std::sync::Arc;

use moa_core::{
    ApprovalRequest, BufferedUserMessage, Event, EventRecord, MoaError, PolicyAction, Result,
    RuntimeEvent, SessionId, SessionMeta, SessionSignal, SessionStatus, SessionStore,
    ToolCallContent, ToolCardStatus, ToolInvocation, ToolUpdate,
};
use moa_hands::ToolRouter;
use moa_security::{InputClassification, check_canary, contains_canary_tokens, inspect_input};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::approval_flow::wait_for_signal_approval;
use super::context_build::append_event;

pub(super) enum ToolCallOutcome {
    Executed,
    Skipped,
    NeedsApproval(ApprovalRequest),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_tool_call(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: Option<&ToolRouter>,
    call: &ToolCallContent,
    active_canary: Option<&str>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<BufferedUserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<ToolCallOutcome> {
    let invocation = &call.invocation;
    let tool_id = parse_tool_id(invocation);
    let serialized_input = serde_json::to_string(&invocation.input)?;

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
                    invocation.name
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
                provider_tool_use_id: invocation.id.clone(),
                tool_name: invocation.name.clone(),
                error: format!(
                    "tool {} blocked because it leaked a protected canary token",
                    invocation.name
                ),
                retryable: false,
            },
        )
        .await?;
        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
            tool_id,
            tool_name: invocation.name.clone(),
            status: ToolCardStatus::Failed,
            summary: format!("{} blocked", invocation.name),
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
                provider_tool_use_id: invocation.id.clone(),
                provider_thought_signature: provider_thought_signature(call),
                tool_name: invocation.name.clone(),
                input: invocation.input.clone(),
                hand_id: None,
            },
        )
        .await?;
        return Ok(ToolCallOutcome::Skipped);
    };

    let prepared = router.prepare_invocation(session, invocation).await?;
    let summary = prepared.input_summary().to_string();

    match &prepared.policy().action {
        PolicyAction::Allow => {
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: invocation.name.clone(),
                status: ToolCardStatus::Running,
                summary,
                detail: None,
            }));
            execute_tool(
                session_id,
                session,
                session_store,
                router,
                invocation,
                tool_id,
                true,
                provider_thought_signature(call).as_deref(),
                active_canary,
                event_tx,
                runtime_tx,
                cancel_token,
                hard_cancel_token,
            )
            .await
        }
        PolicyAction::Deny => {
            record_denied_tool_span(invocation);
            append_event(
                &session_store,
                event_tx,
                session_id.clone(),
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    provider_thought_signature: provider_thought_signature(call),
                    tool_name: invocation.name.clone(),
                    input: invocation.input.clone(),
                    hand_id: None,
                },
            )
            .await?;
            let message = format!("tool {} denied by policy", invocation.name);
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::ToolError {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    tool_name: invocation.name.clone(),
                    error: message.clone(),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: invocation.name.clone(),
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
                    provider_tool_use_id: invocation.id.clone(),
                    provider_thought_signature: provider_thought_signature(call),
                    tool_name: invocation.name.clone(),
                    input: invocation.input.clone(),
                    hand_id: None,
                },
            )
            .await?;
            let prompt = prepared.approval_prompt(tool_id);
            let request = prompt.request.clone();
            append_event(
                &session_store,
                event_tx,
                session_id.clone(),
                Event::ApprovalRequested {
                    request_id: request.request_id,
                    tool_name: request.tool_name.clone(),
                    input_summary: request.input_summary.clone(),
                    risk_level: request.risk_level.clone(),
                    prompt: prompt.clone(),
                },
            )
            .await?;
            session_store
                .update_status(session_id.clone(), SessionStatus::WaitingApproval)
                .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: invocation.name.clone(),
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
                    invocation,
                    tool_id,
                    summary,
                    prepared.policy_input().risk_level.clone(),
                    provider_thought_signature(call).as_deref(),
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
pub(super) async fn execute_tool(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    call: &ToolInvocation,
    tool_id: Uuid,
    emit_call_event: bool,
    provider_thought_signature: Option<&str>,
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
                        provider_thought_signature: provider_thought_signature
                            .map(ToOwned::to_owned),
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
                    provider_tool_use_id: call.id.clone(),
                    tool_name: call.name.clone(),
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
                    provider_tool_use_id: call.id.clone(),
                    tool_name: call.name.clone(),
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
pub(super) async fn execute_pending_tool(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    pending: crate::turn::PendingToolApproval,
    active_canary: Option<&str>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<()> {
    let invocation = ToolInvocation {
        id: resumed_tool_invocation_id(&pending),
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
        pending.provider_thought_signature.as_deref(),
        active_canary,
        event_tx,
        runtime_tx,
        cancel_token,
        hard_cancel_token,
    )
    .await?;
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
        .unwrap_or_else(Uuid::now_v7)
}

fn provider_thought_signature(call: &ToolCallContent) -> Option<String> {
    call.provider_metadata
        .as_ref()
        .and_then(|metadata| metadata.thought_signature())
        .map(ToOwned::to_owned)
}

pub(super) fn resumed_tool_invocation_id(
    pending: &crate::turn::PendingToolApproval,
) -> Option<String> {
    pending
        .provider_tool_use_id
        .clone()
        .or_else(|| Some(pending.tool_id.to_string()))
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

fn record_denied_tool_span(call: &ToolInvocation) {
    let span_name = format!("execute_tool {}", call.name);
    let denied_span = tracing::info_span!(
        "tool_execution",
        otel.name = %span_name,
        gen_ai.tool.name = %call.name,
        gen_ai.tool.call.id = ?call.id,
        moa.tool.success = false,
        moa.tool.denied = true,
        moa.tool.duration_ms = 0i64,
    );
    let _entered = denied_span.enter();
    tracing::info!("tool denied by policy");
}
