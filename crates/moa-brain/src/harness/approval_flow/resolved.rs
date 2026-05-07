//! Processing for approval decisions already persisted in the session log.

use std::sync::Arc;

use moa_core::{
    Event, EventRecord, PolicyAction, Result, RuntimeEvent, SessionId, SessionMeta, SessionStore,
    ToolCardStatus, ToolInvocation, ToolUpdate, UserId,
};
use moa_hands::ToolRouter;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::turn::{StoredApprovalDecision, find_resolved_pending_tool_approval};

use super::super::context_build::append_event;
use super::super::tool_dispatch::{execute_pending_tool, resumed_tool_invocation_id};

#[allow(clippy::too_many_arguments)]
pub(in crate::harness) async fn process_resolved_approval(
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    events: &[EventRecord],
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    tool_dispatch_span: Option<&tracing::Span>,
) -> Result<bool> {
    let Some(pending) = find_resolved_pending_tool_approval(events) else {
        return Ok(false);
    };

    match pending.decision.clone() {
        StoredApprovalDecision::AllowOnce => {
            let invocation = ToolInvocation {
                id: resumed_tool_invocation_id(&pending),
                name: pending.tool_name.clone(),
                input: pending.input.clone(),
            };
            let prepared = tool_router.prepare_invocation(session, &invocation).await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id.0,
                tool_name: pending.tool_name.clone(),
                status: ToolCardStatus::Running,
                summary: prepared.input_summary().to_string(),
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
                tool_dispatch_span,
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
                id: resumed_tool_invocation_id(&pending),
                name: pending.tool_name.clone(),
                input: pending.input.clone(),
            };
            let prepared = tool_router.prepare_invocation(session, &invocation).await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id.0,
                tool_name: pending.tool_name.clone(),
                status: ToolCardStatus::Running,
                summary: prepared.input_summary().to_string(),
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
                tool_dispatch_span,
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
                    provider_tool_use_id: pending.provider_tool_use_id.clone(),
                    tool_name: pending.tool_name.clone(),
                    error: reason
                        .clone()
                        .unwrap_or_else(|| "tool execution denied by user".to_string()),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id.0,
                tool_name: pending.tool_name,
                status: ToolCardStatus::Failed,
                summary: "tool denied".to_string(),
                detail: reason,
            }));
        }
    }

    Ok(true)
}
