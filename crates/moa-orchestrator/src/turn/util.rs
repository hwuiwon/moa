//! Pure turn helpers shared by the durable session and sub-agent runners.

use std::collections::BTreeSet;
use std::time::Duration;

use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, ContextMessage, SessionId,
    StopReason, ToolCallContent, ToolCallId, ToolOutput, TurnOutcome, delegation_tool_schemas,
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

/// Returns the names of tools the provider was allowed to call for one request.
pub(crate) fn allowed_tool_names(request: &CompletionRequest) -> BTreeSet<String> {
    request
        .tools
        .iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Returns whether a provider-emitted tool call is allowed by the compiled request.
pub(crate) fn tool_call_is_allowed(allowed_tools: &BTreeSet<String>, tool_name: &str) -> bool {
    allowed_tools.contains(tool_name)
}

/// Drops empty cancellation reasons so they do not resolve workflow cancellation.
pub(crate) fn meaningful_cancel_reason(reason: Option<String>) -> Option<String> {
    reason.filter(|value| !value.trim().is_empty())
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
    ensure_tool_schema(request, dispatch_sub_agent_tool_schema());
}

/// Ensures the v2 delegation tool schemas are available on the request.
pub(crate) fn ensure_delegation_tool_schemas(request: &mut CompletionRequest) {
    for schema in delegation_tool_schemas() {
        ensure_tool_schema(request, schema);
    }
}

fn ensure_tool_schema(request: &mut CompletionRequest, schema: serde_json::Value) {
    let Some(name) = schema.get("name").and_then(serde_json::Value::as_str) else {
        return;
    };
    let has_tool = request.tools.iter().any(|tool| {
        tool.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|existing| existing == name)
    });
    if !has_tool {
        request.tools.push(schema);
    }
}

/// Builds the synthetic tool output returned when a provider calls a disallowed tool.
pub(crate) fn disallowed_tool_output(tool_name: &str) -> ToolOutput {
    ToolOutput::error(
        format!("Tool {tool_name} is not allowed for this agent turn."),
        Duration::ZERO,
    )
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

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.orchestrator.tool_call_id.v1");
    update_len_prefixed(&mut hasher, session_id.0.as_bytes());
    update_len_prefixed(&mut hasher, &(index as u64).to_be_bytes());
    update_len_prefixed(&mut hasher, tool_call.invocation.name.as_bytes());
    let input = serde_json::to_vec(&tool_call.invocation.input).unwrap_or_default();
    update_len_prefixed(&mut hasher, &input);
    ToolCallId(uuid_from_hash(hasher.finalize()))
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn uuid_from_hash(hash: blake3::Hash) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
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

/// Builds the synthetic tool output used when execution is denied by action policy.
pub(crate) fn denied_tool_output(message: impl Into<String>) -> ToolOutput {
    ToolOutput::error(message.into(), Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use moa_core::{
        CompletionContent, CompletionRequest, CompletionResponse, ModelId, SessionId, TokenUsage,
        ToolInvocation, TurnOutcome,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        allowed_tool_names, disallowed_tool_output, ensure_delegation_tool_schemas,
        ensure_dispatch_tool_schema, meaningful_cancel_reason, stable_tool_call_id,
        summarize_response_text, tool_call_is_allowed, turn_outcome_for_response,
    };

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
    fn meaningful_cancel_reason_ignores_blank_values() {
        // Pins: workflow cancellation promises are not resolved by empty caller input.
        assert_eq!(meaningful_cancel_reason(None), None);
        assert_eq!(meaningful_cancel_reason(Some(String::new())), None);
        assert_eq!(meaningful_cancel_reason(Some("   \n".to_string())), None);
        assert_eq!(
            meaningful_cancel_reason(Some("stop now".to_string())),
            Some("stop now".to_string())
        );
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
        let session_id = SessionId(
            Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .expect("fixture UUID should parse"),
        );
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
        assert_eq!(first.0.to_string(), "cbd69d4a-b3b5-4604-99f0-651dd9dbb308");
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

    #[test]
    fn allowed_tool_names_extracts_only_schema_names() {
        // Pins: runtime tool policy is derived from the compiled request, not from provider output.
        let mut request = CompletionRequest::new("use tools carefully");
        request.tools = vec![
            json!({"name": "file_read", "input_schema": {"type": "object"}}),
            json!({"name": "dispatch_sub_agent", "input_schema": {"type": "object"}}),
            json!({"input_schema": {"type": "object"}}),
            json!({"name": 42, "input_schema": {"type": "object"}}),
        ];

        let allowed = allowed_tool_names(&request);

        assert_eq!(
            allowed,
            BTreeSet::from(["dispatch_sub_agent".to_string(), "file_read".to_string()])
        );
        assert!(tool_call_is_allowed(&allowed, "file_read"));
        assert!(!tool_call_is_allowed(&allowed, "bash"));
    }

    #[test]
    fn disallowed_tool_output_names_the_blocked_tool() {
        // Pins: failed-closed provider tool calls produce a model-visible tool error.
        let output = disallowed_tool_output("bash");

        assert!(output.is_error);
        assert_eq!(
            output.to_text(),
            "Tool bash is not allowed for this agent turn."
        );
    }

    #[test]
    fn delegation_tool_schema_injection_is_idempotent() {
        // Pins: v2 delegation tools are injected once and recognized as runner-handled tools.
        let mut request = CompletionRequest::new("delegate");

        ensure_dispatch_tool_schema(&mut request);
        ensure_delegation_tool_schemas(&mut request);
        ensure_delegation_tool_schemas(&mut request);

        let names = allowed_tool_names(&request);
        assert_eq!(names.len(), 6);
        assert!(moa_core::is_delegation_tool_name("dispatch_sub_agent"));
        assert!(moa_core::is_delegation_tool_name("spawn_sub_agent"));
        assert!(moa_core::is_delegation_tool_name("wait_sub_agent"));
        assert!(moa_core::is_delegation_tool_name("message_sub_agent"));
        assert!(moa_core::is_delegation_tool_name("list_sub_agents"));
        assert!(moa_core::is_delegation_tool_name("cancel_sub_agent"));
    }
}
