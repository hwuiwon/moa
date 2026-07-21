//! Tool call dispatch, execution, and output handling for the harness.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    error::MoaError, error::Result, events::Event, traits::Identity, traits::SessionStore,
    types::action_policy::ActionPolicyEffect, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::events_stream::EventRecord,
    types::identifiers::SessionId, types::identifiers::ToolCallId,
    types::runtime_events::RuntimeEvent, types::runtime_events::ToolCardStatus,
    types::runtime_events::ToolUpdate, types::session::SessionMeta,
};
use moa_hands::ToolRouter;
use moa_security::{InputClassification, ToolInputCanaryScreening, inspect_input};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use super::context_build::append_event;

pub(super) enum ToolCallOutcome {
    Executed,
    Skipped(Option<ToolFailure>),
    Cancelled,
}

/// A terminal (non-retryable) tool failure captured for negative-results memory.
///
/// `error_class` is a stable, low-cardinality label (never a raw error message),
/// so incident titles assembled from it deduplicate across turns.
pub(super) struct ToolFailure {
    pub(super) tool_name: String,
    pub(super) error_class: &'static str,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_tool_call(
    caller_identity: &Identity,
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
    tool_dispatch_span: Option<&tracing::Span>,
) -> Result<ToolCallOutcome> {
    let invocation = &call.invocation;
    let tool_id = parse_tool_id(invocation);
    let serialized_input = serde_json::to_string(&invocation.input)?;

    if matches!(
        moa_security::screen_tool_input_for_canary(active_canary, &serialized_input),
        ToolInputCanaryScreening::Blocked(_)
    ) {
        append_tool_call_event(
            &session_store,
            event_tx,
            session_id,
            tool_id,
            invocation,
            provider_thought_signature(call).as_deref(),
            None,
        )
        .await?;
        append_event(
            &session_store,
            event_tx,
            session_id,
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
            tool_id: tool_id.0,
            tool_name: invocation.name.clone(),
            status: ToolCardStatus::Failed,
            summary: format!("{} blocked", invocation.name),
            detail: Some(
                "Blocked because a protected canary token leaked into tool input".to_string(),
            ),
        }));
        return Ok(ToolCallOutcome::Skipped(Some(ToolFailure {
            tool_name: invocation.name.clone(),
            error_class: "canary_leak",
        })));
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
        return Ok(ToolCallOutcome::Skipped(None));
    };

    let prepared = router.prepare_invocation(session, invocation).await?;
    let summary = prepared.input_summary().to_string();

    match &prepared.policy().effect {
        ActionPolicyEffect::Allow => {
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: tool_id.0,
                tool_name: invocation.name.clone(),
                status: ToolCardStatus::Running,
                summary,
                detail: None,
            }));
            execute_tool(
                caller_identity,
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
                tool_dispatch_span,
            )
            .await
        }
        ActionPolicyEffect::Deny => {
            record_denied_tool_span(invocation, tool_dispatch_span);
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
            let message = format!(
                "tool {} denied by action policy: {}",
                invocation.name,
                prepared.policy().reason.as_deref().unwrap_or("no reason")
            );
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
                tool_id: tool_id.0,
                tool_name: invocation.name.clone(),
                status: ToolCardStatus::Failed,
                summary,
                detail: Some(message),
            }));
            Ok(ToolCallOutcome::Skipped(Some(ToolFailure {
                tool_name: invocation.name.clone(),
                error_class: "action_policy_denied",
            })))
        }
        ActionPolicyEffect::AdminReview => {
            record_denied_tool_span(invocation, tool_dispatch_span);
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
            let message = format!(
                "tool {} requires tenant admin review, but the local brain harness does not have a durable action-review queue: {}",
                invocation.name, summary
            );
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
                tool_id: tool_id.0,
                tool_name: invocation.name.clone(),
                status: ToolCardStatus::Failed,
                summary: summary.clone(),
                detail: Some(message),
            }));
            Ok(ToolCallOutcome::Skipped(Some(ToolFailure {
                tool_name: invocation.name.clone(),
                error_class: "admin_review_required",
            })))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_tool(
    caller_identity: &Identity,
    session_id: SessionId,
    session: &SessionMeta,
    session_store: Arc<dyn SessionStore>,
    tool_router: &ToolRouter,
    call: &ToolInvocation,
    tool_id: ToolCallId,
    emit_call_event: bool,
    provider_thought_signature: Option<&str>,
    active_canary: Option<&str>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    cancel_token: Option<&CancellationToken>,
    hard_cancel_token: Option<&CancellationToken>,
    tool_dispatch_span: Option<&tracing::Span>,
) -> Result<ToolCallOutcome> {
    if emit_call_event {
        append_tool_call_event(
            &session_store,
            event_tx,
            session_id,
            tool_id,
            call,
            provider_thought_signature,
            None,
        )
        .await?;
    }

    let span_name = format!("tool:{}", call.name);
    let current_span = tracing::Span::current();
    let parent_span = tool_dispatch_span.unwrap_or(&current_span);
    let tool_span = tracing::info_span!(
        parent: parent_span,
        "tool_execution",
        otel.name = %span_name,
        gen_ai.tool.name = %call.name,
        gen_ai.tool.call.id = ?call.id,
        moa.tool.success = tracing::field::Empty,
        moa.tool.denied = false,
        moa.tool.duration_ms = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let mut execution_call = call.clone();
    execution_call.id = Some(tool_id.to_string());
    let execution_result = tool_router
        .execute_authorized_with_cancel(
            session,
            caller_identity,
            &execution_call,
            cancel_token,
            hard_cancel_token,
        )
        .instrument(tool_span.clone())
        .await;
    let duration_ms = started_at.elapsed().as_millis() as i64;
    tool_span.record("moa.tool.duration_ms", duration_ms);

    match execution_result {
        Ok((_resolved_hand_id, output)) => {
            tool_span.record("moa.tool.success", true);
            let secured_output = secure_tool_output(&output, active_canary);
            emit_tool_output_warning(
                session_id,
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
                    original_output_tokens: output.original_output_tokens,
                    success: !output.is_error,
                    duration_ms: output.duration.as_millis() as u64,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: tool_id.0,
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
            // Cancellation is not a durable failure: the turn was interrupted, so
            // it carries no negative-results lesson to preserve.
            tool_span.record("moa.tool.success", false);
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
                tool_id: tool_id.0,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary: format!("{} cancelled", call.name),
                detail: Some("cancelled".to_string()),
            }));
            Ok(ToolCallOutcome::Cancelled)
        }
        Err(ref error) => {
            tool_span.record("moa.tool.success", false);
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
                tool_id: tool_id.0,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary: format!("{} failed", call.name),
                detail: Some(error.to_string()),
            }));
            Ok(ToolCallOutcome::Skipped(Some(ToolFailure {
                tool_name: call.name.clone(),
                error_class: tool_error_class(error),
            })))
        }
    }
}

/// Maps a terminal tool error to a stable, low-cardinality class label.
///
/// Titles derived from these labels must deduplicate across turns, so the label
/// is drawn from the error variant, never from its (unbounded) message text.
fn tool_error_class(error: &MoaError) -> &'static str {
    match error {
        MoaError::ToolError(_) => "tool_error",
        MoaError::ValidationError(_) => "validation_error",
        MoaError::PermissionDenied(_) => "permission_denied",
        MoaError::BudgetExhausted(_) => "budget_exhausted",
        MoaError::RateLimited { .. } => "rate_limited",
        MoaError::Unsupported(_) | MoaError::NotImplemented(_) => "unsupported",
        MoaError::ProviderError(_) | MoaError::ProviderQuirk(_) | MoaError::StreamError(_) => {
            "provider_error"
        }
        _ => "tool_failure",
    }
}

async fn append_tool_call_event(
    session_store: &Arc<dyn SessionStore>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    provider_thought_signature: Option<&str>,
    hand_id: Option<String>,
) -> Result<()> {
    append_event(
        session_store,
        event_tx,
        session_id,
        Event::ToolCall {
            tool_id,
            provider_tool_use_id: invocation.id.clone(),
            provider_thought_signature: provider_thought_signature.map(ToOwned::to_owned),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            hand_id,
        },
    )
    .await
}

fn format_tool_output(output: &moa_core::types::tools::ToolOutput) -> String {
    output.to_text()
}

fn summarize_tool_completion(
    call: &ToolInvocation,
    output: &moa_core::types::tools::ToolOutput,
) -> String {
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

fn parse_tool_id(call: &ToolInvocation) -> ToolCallId {
    call.id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .map(ToolCallId::from)
        .unwrap_or_default()
}

fn provider_thought_signature(call: &ToolCallContent) -> Option<String> {
    call.provider_metadata
        .as_ref()
        .and_then(|metadata| metadata.thought_signature())
        .map(ToOwned::to_owned)
}

struct SecuredToolOutput {
    inspection: moa_security::InputInspection,
}

fn secure_tool_output(
    output: &moa_core::types::tools::ToolOutput,
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
    tool_id: ToolCallId,
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

fn record_denied_tool_span(call: &ToolInvocation, tool_dispatch_span: Option<&tracing::Span>) {
    let span_name = format!("tool:{}", call.name);
    let current_span = tracing::Span::current();
    let parent_span = tool_dispatch_span.unwrap_or(&current_span);
    let denied_span = tracing::info_span!(
        parent: parent_span,
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

#[cfg(test)]
mod tests {
    use super::tool_error_class;
    use moa_core::error::MoaError;

    #[test]
    fn tool_error_class_is_stable_and_low_cardinality() {
        // Pins: terminal tool errors map to a fixed label drawn from the error
        // variant (never the message), so incident titles built from tool name +
        // class deduplicate across turns instead of exploding on error text.
        assert_eq!(
            tool_error_class(&MoaError::ToolError("boom: id=4821".to_string())),
            "tool_error"
        );
        assert_eq!(
            tool_error_class(&MoaError::ToolError("boom: id=9999".to_string())),
            "tool_error",
            "message contents must not change the class"
        );
        assert_eq!(
            tool_error_class(&MoaError::PermissionDenied("no".to_string())),
            "permission_denied"
        );
        assert_eq!(
            tool_error_class(&MoaError::ProviderError("upstream".to_string())),
            "provider_error"
        );
        assert_eq!(
            tool_error_class(&MoaError::HomeDirectoryNotFound),
            "tool_failure",
            "unmapped variants fall back to the generic terminal-failure label"
        );
    }
}
