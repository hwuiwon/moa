//! Shared approval gate for durable session and sub-agent turns.

use std::time::{Duration, Instant};

use chrono::Utc;
use moa_core::{
    ApprovalDecision, ApprovalPrompt, Event, PolicyAction, SessionId, SessionMeta, ToolCallId,
    ToolInvocation, ToolOutput, record_approval_wait, record_turn_event_persist_duration,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use super::adapter::AgentAdapter;
use super::event_persist_span;
use super::util::denied_tool_output;
use crate::services::{
    session_store::{AppendEventRequest, RestateSessionStoreClient},
    workspace_store::{StoreApprovalRuleRequest, WorkspaceStoreClient},
};

const APPROVAL_TIMEOUT_SECS_ENV: &str = "MOA_APPROVAL_TIMEOUT_SECS";
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;

/// Result returned by the shared approval gate.
pub(crate) struct ApprovalOutcome {
    /// Whether tool execution may proceed.
    pub allow_execution: bool,
    /// Synthetic tool output recorded when execution was denied.
    pub denied_output: ToolOutput,
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

/// Runs the durable approval gate for one tool invocation.
pub(crate) async fn handle_approval_gate<A: AgentAdapter>(
    ctx: &mut ObjectContext<'_>,
    adapter: &A,
    session_id: SessionId,
    meta: &SessionMeta,
    invocation: &ToolInvocation,
    _tool_id: ToolCallId,
    prompt: Option<ApprovalPrompt>,
) -> Result<ApprovalOutcome, HandlerError> {
    let mut prompt = prompt.ok_or_else(|| {
        TerminalError::new(format!(
            "workspace store did not return an approval prompt for tool {}",
            invocation.name
        ))
    })?;
    let sub_agent_id = adapter.sub_agent_id(ctx);
    let (awakeable_id, awakeable) = ctx.awakeable::<String>();

    adapter
        .set_pending_approval(ctx, awakeable_id.clone())
        .await?;

    prompt.request.sub_agent_id = sub_agent_id.clone();
    append_session_event(
        ctx,
        session_id,
        Event::ApprovalRequested {
            request_id: prompt.request.request_id,
            awakeable_id: Some(awakeable_id),
            sub_agent_id: sub_agent_id.clone(),
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

    adapter.clear_pending_approval(ctx).await?;

    let decided_by = match &decision {
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason == &timed_out_reason => "system:auto-timeout".to_string(),
        _ => meta.user_id.to_string(),
    };

    append_session_event(
        ctx,
        session_id,
        Event::ApprovalDecided {
            request_id: prompt.request.request_id,
            sub_agent_id,
            decision: decision.clone(),
            decided_by,
            decided_at: Utc::now(),
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

/// Serializes an approval decision for a Restate awakeable payload.
pub(crate) fn serialize_awakeable_decision(
    decision: &ApprovalDecision,
) -> Result<String, TerminalError> {
    serde_json::to_string(decision).map_err(|error| {
        TerminalError::new(format!(
            "failed to serialize approval decision for awakeable: {error}"
        ))
    })
}

async fn append_session_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(())
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
        ApprovalDecision::Deny { .. } => "deny",
    }
}

fn parse_awakeable_decision(raw: &str) -> Result<ApprovalDecision, TerminalError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize approval decision from awakeable: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::ApprovalDecision;

    use super::{
        approval_wait_timeout_from_env, parse_awakeable_decision, serialize_awakeable_decision,
    };

    #[test]
    fn approval_timeout_defaults_when_override_is_missing_or_invalid() {
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
    }

    #[test]
    fn approval_timeout_uses_positive_override() {
        assert_eq!(
            approval_wait_timeout_from_env(Some("45"), 1800),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn awakeable_decision_round_trips_through_json_payload() {
        let encoded = serialize_awakeable_decision(&ApprovalDecision::AlwaysAllow {
            pattern: "bash printf*".to_string(),
        })
        .expect("serialize approval decision");
        let decoded = parse_awakeable_decision(&encoded).expect("deserialize approval decision");

        assert_eq!(
            decoded,
            ApprovalDecision::AlwaysAllow {
                pattern: "bash printf*".to_string(),
            }
        );
    }
}
