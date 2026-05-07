//! Live-signal approval waits for newly requested tool calls.

use std::sync::Arc;

use moa_core::{
    ApprovalDecision, BufferedUserMessage, Event, EventRecord, MoaError, PolicyAction, Result,
    RiskLevel, RuntimeEvent, SessionId, SessionMeta, SessionSignal, SessionStatus, SessionStore,
    ToolCallId, ToolCardStatus, ToolInvocation, ToolUpdate,
};
use moa_hands::ToolRouter;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::super::context_build::{append_event, approval_decision_label, buffer_queued_message};
use super::super::tool_dispatch::{ToolCallOutcome, execute_tool};

#[allow(clippy::too_many_arguments)]
pub(in crate::harness) async fn wait_for_signal_approval(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    call: &ToolInvocation,
    tool_id: ToolCallId,
    summary: String,
    risk_level: RiskLevel,
    provider_thought_signature: Option<&str>,
    active_canary: Option<&str>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    tool_dispatch_span: Option<&tracing::Span>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<BufferedUserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<ToolCallOutcome> {
    let approval_span = tracing::info_span!(
        "approval_wait",
        moa.approval.tool = %call.name,
        moa.approval.risk_level = ?risk_level,
        moa.approval.decision = tracing::field::Empty,
    );

    let instrument_approval_span = approval_span.clone();
    async move {
        loop {
            match signal_rx.recv().await {
                Some(SessionSignal::ApprovalDecided {
                    request_id,
                    decision,
                }) if request_id == tool_id.0 => {
                    approval_span.record(
                        "moa.approval.decision",
                        tracing::field::display(approval_decision_label(&decision)),
                    );
                    append_event(
                        &session_store,
                        event_tx,
                        session_id,
                        Event::ApprovalDecided {
                            request_id,
                            sub_agent_id: None,
                            decision: decision.clone(),
                            decided_by: session.user_id.to_string(),
                            decided_at: chrono::Utc::now(),
                        },
                    )
                    .await?;
                    if let Some(record) = session_store
                        .transition_status(session_id, SessionStatus::Running)
                        .await?
                        && let Some(event_tx) = event_tx
                    {
                        let _ = event_tx.send(record);
                    }

                    return match decision {
                        ApprovalDecision::AllowOnce => {
                            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                                tool_id: tool_id.0,
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
                                provider_thought_signature,
                                active_canary,
                                event_tx,
                                runtime_tx,
                                cancel_token,
                                hard_cancel_token,
                                tool_dispatch_span,
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
                                tool_id: tool_id.0,
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
                                provider_thought_signature,
                                active_canary,
                                event_tx,
                                runtime_tx,
                                cancel_token,
                                hard_cancel_token,
                                tool_dispatch_span,
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
                                    provider_tool_use_id: call.id.clone(),
                                    tool_name: call.name.clone(),
                                    error: reason.clone().unwrap_or_else(|| {
                                        "tool execution denied by user".to_string()
                                    }),
                                    retryable: false,
                                },
                            )
                            .await?;
                            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                                tool_id: tool_id.0,
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
                    approval_span.record("moa.approval.decision", "soft_cancel");
                    let _ = runtime_tx.send(RuntimeEvent::Notice(
                        "Stop requested. MOA will stop after the current step.".to_string(),
                    ));
                    return Ok(ToolCallOutcome::Cancelled);
                }
                Some(SessionSignal::HardCancel) => {
                    *soft_cancel_requested = true;
                    approval_span.record("moa.approval.decision", "hard_cancel");
                    return Ok(ToolCallOutcome::Cancelled);
                }
                Some(SessionSignal::ApprovalDecided { .. }) => {}
                None => {
                    approval_span.record("moa.approval.decision", "channel_closed");
                    return Err(MoaError::ProviderError(
                        "approval channel closed".to_string(),
                    ));
                }
            }
        }
    }
    .instrument(instrument_approval_span)
    .await
}
