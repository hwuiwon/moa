//! Pure turn helpers shared by the durable session and sub-agent runners.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, ContextMessage, SessionId,
    StopReason, ToolCallContent, ToolCallId, ToolOutput, TurnOutcome,
    dispatch_sub_agent_tool_schema,
};
use uuid::Uuid;

/// Returns the structured tool calls emitted in one completion response.
pub(crate) fn response_tool_calls(response: &CompletionResponse) -> Vec<&ToolCallContent> {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            CompletionContent::ToolCall(tool_call) => Some(tool_call),
            CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => None,
        })
        .collect()
}

/// Maps one completion response into the next durable turn outcome.
pub(crate) fn turn_outcome_for_response(response: &CompletionResponse) -> TurnOutcome {
    if !response_tool_calls(response).is_empty() || response.stop_reason == StopReason::ToolUse {
        return TurnOutcome::Continue;
    }

    if response.stop_reason == StopReason::Cancelled {
        return TurnOutcome::Cancelled;
    }

    TurnOutcome::Idle
}

/// Produces a short summary string from visible assistant text.
pub(crate) fn summarize_response_text(response: &CompletionResponse) -> Option<String> {
    let trimmed = response.text.trim();
    if trimmed.is_empty() {
        return None;
    }

    const MAX_SUMMARY_CHARS: usize = 240;
    Some(trimmed.chars().take(MAX_SUMMARY_CHARS).collect())
}

/// Ensures the shared `dispatch_sub_agent` schema is available on the request.
pub(crate) fn ensure_dispatch_tool_schema(request: &mut CompletionRequest) {
    let has_dispatch_tool = request.tools.iter().any(|tool| {
        tool.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name == "dispatch_sub_agent")
    });
    if !has_dispatch_tool {
        request.tools.push(dispatch_sub_agent_tool_schema());
    }
}

/// Computes a stable tool-call identifier from provider output.
pub(crate) fn stable_tool_call_id(
    session_id: SessionId,
    index: usize,
    tool_call: &ToolCallContent,
) -> ToolCallId {
    if let Some(raw_id) = tool_call.invocation.id.as_deref()
        && let Ok(uuid) = Uuid::parse_str(raw_id)
    {
        return ToolCallId(uuid);
    }

    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    index.hash(&mut hasher);
    tool_call.invocation.name.hash(&mut hasher);
    tool_call.invocation.input.to_string().hash(&mut hasher);
    ToolCallId(Uuid::from_u128(hasher.finish() as u128))
}

/// Appends the provider response into a sub-agent's local history buffer.
pub(crate) fn apply_response_to_history(
    history: &mut Vec<ContextMessage>,
    response: &CompletionResponse,
) {
    let mut appended_text = false;
    for block in &response.content {
        match block {
            CompletionContent::Text(text) if !text.trim().is_empty() => {
                history.push(ContextMessage::assistant_with_thought_signature(
                    text.clone(),
                    response.thought_signature.clone(),
                ));
                appended_text = true;
            }
            CompletionContent::ToolCall(tool_call) => {
                history.push(ContextMessage::assistant_tool_call_with_thought_signature(
                    tool_call.invocation.clone(),
                    if response.text.trim().is_empty() {
                        format!("Calling tool {}", tool_call.invocation.name)
                    } else {
                        response.text.clone()
                    },
                    response.thought_signature.clone(),
                ));
            }
            CompletionContent::ProviderToolResult { tool_name, summary } => {
                history.push(ContextMessage::assistant(format!("{tool_name}: {summary}")));
                appended_text = true;
            }
            CompletionContent::Text(_) => {}
        }
    }

    if !appended_text
        && !response.text.trim().is_empty()
        && response_tool_calls(response).is_empty()
    {
        history.push(ContextMessage::assistant_with_thought_signature(
            response.text.clone(),
            response.thought_signature.clone(),
        ));
    }
}

/// Builds the synthetic tool output used when execution is denied after approval.
pub(crate) fn denied_tool_output(message: impl Into<String>) -> ToolOutput {
    ToolOutput::error(message.into(), Duration::ZERO)
}

/// Returns the assistant-tool-call status text for one dispatched sub-agent result.
pub(crate) fn dispatch_history_text(output: &ToolOutput) -> String {
    let rendered = output.to_text();
    if let Some(remainder) = rendered.strip_prefix("Sub-agent ")
        && let Some((sub_agent_id, _)) = remainder.split_once(' ')
    {
        return format!("Dispatching sub-agent for {sub_agent_id}");
    }

    "Calling tool dispatch_sub_agent".to_string()
}

#[cfg(test)]
mod tests {
    use moa_core::{
        CompletionContent, CompletionResponse, ModelId, SessionId, TokenUsage, ToolInvocation,
        TurnOutcome,
    };
    use serde_json::json;

    use super::{stable_tool_call_id, summarize_response_text, turn_outcome_for_response};

    fn completion_response(
        text: &str,
        content: Vec<CompletionContent>,
        stop_reason: moa_core::StopReason,
    ) -> CompletionResponse {
        CompletionResponse {
            text: text.to_string(),
            content,
            stop_reason,
            model: ModelId::new("test-model"),
            usage: TokenUsage::default(),
            duration_ms: 0,
            thought_signature: None,
        }
    }

    #[test]
    fn tool_use_response_continues_the_turn() {
        let response = completion_response(
            "working",
            vec![CompletionContent::ToolCall(moa_core::ToolCallContent {
                invocation: ToolInvocation {
                    id: Some("provider-tool-id".to_string()),
                    name: "file_read".to_string(),
                    input: json!({"path":"/tmp/test.txt"}),
                },
                provider_metadata: None,
            })],
            moa_core::StopReason::ToolUse,
        );

        assert_eq!(turn_outcome_for_response(&response), TurnOutcome::Continue);
    }

    #[test]
    fn cancelled_response_maps_to_cancelled_outcome() {
        let response = completion_response(
            "",
            vec![CompletionContent::Text(String::new())],
            moa_core::StopReason::Cancelled,
        );

        assert_eq!(turn_outcome_for_response(&response), TurnOutcome::Cancelled);
    }

    #[test]
    fn stable_tool_call_id_is_deterministic() {
        let session_id = SessionId::new();
        let call = moa_core::ToolCallContent {
            invocation: ToolInvocation {
                id: Some("provider-tool-id".to_string()),
                name: "bash".to_string(),
                input: json!({"command":"echo hello"}),
            },
            provider_metadata: None,
        };

        let first = stable_tool_call_id(session_id, 0, &call);
        let second = stable_tool_call_id(session_id, 0, &call);
        let third = stable_tool_call_id(session_id, 1, &call);

        assert_eq!(first, second);
        assert_ne!(first, third);
    }

    #[test]
    fn summarize_response_text_trims_and_limits() {
        let response = completion_response(
            &"a".repeat(300),
            vec![CompletionContent::Text("ok".to_string())],
            moa_core::StopReason::EndTurn,
        );

        let summary = summarize_response_text(&response).expect("summary should exist");
        assert_eq!(summary.len(), 240);
    }
}
