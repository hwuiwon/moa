//! Pure turn helpers shared by the durable session and worker runners.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use moa_brain::segment_assessment::verification_signal::{self, VerificationKind};
use moa_core::{
    error::Result, types::completion::CompletionContent, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::completion::StopReason,
    types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::context::ContextMessage, types::identifiers::SessionId, types::identifiers::ToolCallId,
    types::session::TurnOutcome, types::tools::ToolOutput,
    types::worker::tool_schema::delegation_tool_schemas,
};
use moa_security::{ToolInputCanaryScreening, screen_tool_input_for_canary};
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnEvidence {
    failed_verifications: BTreeMap<VerificationKind, VerificationFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerificationFailure {
    pub(crate) kind: VerificationKind,
    pub(crate) tool_name: String,
    pub(crate) command: Option<String>,
    pub(crate) exit_code: Option<i32>,
}

impl VerificationFailure {
    fn annotation(&self) -> String {
        let subject = self.command.as_deref().unwrap_or(&self.tool_name);
        if let Some(exit_code) = self.exit_code {
            return format!(
                "Verification not green: {subject} exited {exit_code} this turn and was not rerun successfully."
            );
        }

        format!(
            "Verification not green: {subject} failed this turn and was not rerun successfully."
        )
    }
}

impl TurnEvidence {
    /// Records whether one completed tool call changed the turn's verification state.
    pub(crate) fn record_tool_result(&mut self, invocation: &ToolInvocation, output: &ToolOutput) {
        let Some(kind) = verification_signal::classify_tool_input(&invocation.input) else {
            return;
        };

        if output.is_error {
            self.failed_verifications.insert(
                kind,
                VerificationFailure {
                    kind,
                    tool_name: invocation.name.clone(),
                    command: verification_command_summary(&invocation.input),
                    exit_code: output.process_exit_code(),
                },
            );
            return;
        }

        // This is intentionally coarse: any later success in the same verification class clears
        // an earlier failure of that class.
        self.failed_verifications.remove(&kind);
    }

    pub(crate) fn failed_verification(&self) -> Option<&VerificationFailure> {
        self.failed_verifications.values().next()
    }
}

/// Appends unresolved verification evidence to an idle response without replacing the response.
pub(crate) fn annotate_unresolved_verification(
    response: &CompletionResponse,
    evidence: &TurnEvidence,
) -> (CompletionResponse, bool) {
    if turn_outcome_for_response(response) != TurnOutcome::Idle {
        return (response.clone(), false);
    }
    let Some(failure) = evidence.failed_verification() else {
        return (response.clone(), false);
    };

    let note = failure.annotation();
    let mut annotated = response.clone();
    annotated.text = append_verification_note(&response.text, &note);
    annotated.content.push(CompletionContent::Text(note));
    annotated.thought_signature = None;
    (annotated, true)
}

fn append_verification_note(text: &str, note: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return note.to_string();
    }

    format!("{trimmed}\n\n{note}")
}

fn verification_command_summary(input: &serde_json::Value) -> Option<String> {
    let raw = input
        .get("cmd")
        .or_else(|| input.get("command"))
        .or_else(|| input.get("input"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }

    Some(truncate_chars(&normalized, 160))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
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

/// Ensures the v2 delegation tool schemas are available on the request.
pub(crate) fn ensure_delegation_tool_schemas(request: &mut CompletionRequest) {
    for schema in delegation_tool_schemas() {
        ensure_tool_schema(request, schema);
    }
}

/// Removes operator-facing execution lifecycle controls from a provider request.
pub(crate) fn exclude_execution_lifecycle_tool_schemas(request: &mut CompletionRequest) {
    request.tools.retain(|schema| {
        schema
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|name| !is_execution_lifecycle_tool_name(name))
    });
}

fn is_execution_lifecycle_tool_name(name: &str) -> bool {
    matches!(
        name,
        "execution_runs_list"
            | "execution_run_start"
            | "execution_run_status"
            | "execution_run_cancel"
            | "execution_review_decide"
            | "execution_signal"
    )
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
    denied_tool_output(format!(
        "Tool {tool_name} is not allowed for this agent turn."
    ))
}

/// Returns whether serialized tool input contains a protected canary marker.
pub(crate) fn tool_input_leaks_canary(
    active_canary: Option<&str>,
    input: &serde_json::Value,
) -> Result<bool> {
    let serialized_input = serde_json::to_string(input)?;
    Ok(matches!(
        screen_tool_input_for_canary(active_canary, &serialized_input),
        ToolInputCanaryScreening::Blocked(_)
    ))
}

/// Builds the synthetic tool output used when execution leaks a canary.
pub(crate) fn blocked_canary_tool_output(tool_name: &str) -> ToolOutput {
    denied_tool_output(blocked_canary_message(tool_name))
}

/// Returns the model-visible canary block message for a tool.
pub(crate) fn blocked_canary_message(tool_name: &str) -> String {
    format!("Tool {tool_name} blocked because it leaked a protected canary token.")
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
    hasher.update(b"moa.orchestrator.tool_call_id.v2");
    update_len_prefixed(&mut hasher, session_id.0.as_bytes());
    update_len_prefixed(&mut hasher, &(index as u64).to_be_bytes());
    if let Some(raw_id) = tool_call.invocation.id.as_deref() {
        update_len_prefixed(&mut hasher, raw_id.as_bytes());
    }
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

/// Appends the provider response into a worker's local history buffer.
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
        types::completion::CompletionContent, types::completion::CompletionRequest,
        types::completion::CompletionResponse, types::completion::TokenUsage,
        types::completion::ToolInvocation, types::identifiers::ModelId,
        types::identifiers::SessionId, types::session::TurnOutcome,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        TurnEvidence, allowed_tool_names, annotate_unresolved_verification, disallowed_tool_output,
        ensure_delegation_tool_schemas, meaningful_cancel_reason, stable_tool_call_id,
        summarize_response_text, tool_input_leaks_canary, turn_outcome_for_response,
    };

    fn completion_response(
        text: &str,
        content: Vec<CompletionContent>,
        stop_reason: moa_core::types::completion::StopReason,
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
            vec![CompletionContent::ToolCall(
                moa_core::types::completion::ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("provider-tool-id".to_string()),
                        name: "file_read".to_string(),
                        input: json!({"path":"/tmp/test.txt"}),
                    },
                    provider_metadata: None,
                },
            )],
            moa_core::types::completion::StopReason::ToolUse,
        );

        assert_eq!(turn_outcome_for_response(&response), TurnOutcome::Continue);
    }

    #[test]
    fn turn_evidence_records_failed_verification() {
        // Pins: failed deterministic checks retain detail for later response annotation.
        let mut evidence = TurnEvidence::default();
        evidence.record_tool_result(
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({"cmd": "cargo test -p moa-orchestrator"}),
            },
            &moa_core::types::tools::ToolOutput::from_process(
                String::new(),
                "test failed".to_string(),
                1,
                std::time::Duration::ZERO,
            ),
        );

        let failure = evidence
            .failed_verification()
            .expect("failed verification should be retained");
        assert_eq!(failure.tool_name, "bash");
        assert_eq!(
            failure.command.as_deref(),
            Some("cargo test -p moa-orchestrator")
        );
        assert_eq!(failure.exit_code, Some(1));
    }

    #[test]
    fn idle_response_with_failed_verification_is_annotated() {
        // Pins: unresolved failed verification adds a factual note without replacing the response.
        let mut evidence = TurnEvidence::default();
        evidence.record_tool_result(
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({"cmd": "cargo test -p moa-orchestrator"}),
            },
            &moa_core::types::tools::ToolOutput::from_process(
                String::new(),
                "test failed".to_string(),
                1,
                std::time::Duration::ZERO,
            ),
        );
        let response = completion_response(
            "I couldn't finish the fix.",
            vec![CompletionContent::Text(
                "I couldn't finish the fix.".to_string(),
            )],
            moa_core::types::completion::StopReason::EndTurn,
        );

        let (visible, annotated) = annotate_unresolved_verification(&response, &evidence);

        assert!(annotated);
        assert_eq!(
            visible.text,
            "I couldn't finish the fix.\n\nVerification not green: cargo test -p moa-orchestrator exited 1 this turn and was not rerun successfully."
        );
        assert_eq!(
            visible.stop_reason,
            moa_core::types::completion::StopReason::EndTurn
        );
        assert_eq!(visible.thought_signature, None);
    }

    #[test]
    fn tool_call_response_with_failed_verification_is_not_annotated() {
        // Pins: the deterministic gate never suppresses a response that continues with tools.
        let mut evidence = TurnEvidence::default();
        evidence.record_tool_result(
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({"cmd": "cargo test"}),
            },
            &moa_core::types::tools::ToolOutput::from_process(
                String::new(),
                String::new(),
                1,
                std::time::Duration::ZERO,
            ),
        );
        let response = completion_response(
            "I'll rerun it.",
            vec![CompletionContent::ToolCall(
                moa_core::types::completion::ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("provider-tool-id".to_string()),
                        name: "bash".to_string(),
                        input: json!({"cmd":"cargo test"}),
                    },
                    provider_metadata: None,
                },
            )],
            moa_core::types::completion::StopReason::ToolUse,
        );

        let (visible, annotated) = annotate_unresolved_verification(&response, &evidence);

        assert!(!annotated);
        assert_eq!(visible, response);
    }

    #[test]
    fn later_success_clears_same_kind_failed_verification() {
        // Pins: agents may claim completion after rerunning the failed class of verification successfully.
        let mut evidence = TurnEvidence::default();
        let invocation = ToolInvocation {
            id: None,
            name: "bash".to_string(),
            input: json!({"cmd": "cargo test"}),
        };
        evidence.record_tool_result(
            &invocation,
            &moa_core::types::tools::ToolOutput::from_process(
                String::new(),
                String::new(),
                1,
                std::time::Duration::ZERO,
            ),
        );
        evidence.record_tool_result(
            &invocation,
            &moa_core::types::tools::ToolOutput::from_process(
                "ok".to_string(),
                String::new(),
                0,
                std::time::Duration::ZERO,
            ),
        );

        assert_eq!(evidence.failed_verification(), None);
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
            moa_core::types::completion::StopReason::Cancelled,
        );

        assert_eq!(turn_outcome_for_response(&response), TurnOutcome::Cancelled);
    }

    #[test]
    fn tool_input_leaks_canary_detects_active_marker() {
        // Pins: admin-review paths share the same canary screening as direct execution.
        let input = json!({"cmd":"printf moa_canary_test"});
        assert!(tool_input_leaks_canary(None, &input).expect("canary screen should serialize"));
    }

    #[test]
    fn stable_tool_call_id_is_deterministic() {
        let session_id = SessionId(
            Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .expect("fixture UUID should parse"),
        );
        let call = moa_core::types::completion::ToolCallContent {
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
        let mut repeated_call = call.clone();
        repeated_call.invocation.id = Some("provider-tool-id-2".to_string());
        let fourth = stable_tool_call_id(session_id, 0, &repeated_call);

        assert_eq!(first, second);
        assert_ne!(first, third);
        assert_ne!(first, fourth);
        assert_eq!(first.0.to_string(), "b9a3e70e-6e0e-49f8-9034-2405d5019a72");
    }

    #[test]
    fn summarize_response_text_trims_and_limits() {
        let response = completion_response(
            &"a".repeat(300),
            vec![CompletionContent::Text("ok".to_string())],
            moa_core::types::completion::StopReason::EndTurn,
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
            json!({"input_schema": {"type": "object"}}),
            json!({"name": 42, "input_schema": {"type": "object"}}),
        ];

        let allowed = allowed_tool_names(&request);

        assert_eq!(allowed, BTreeSet::from(["file_read".to_string()]));
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
        // Pins: delegation tools are injected once and recognized as runner-handled tools.
        let mut request = CompletionRequest::new("delegate");

        ensure_delegation_tool_schemas(&mut request);
        ensure_delegation_tool_schemas(&mut request);

        let names = allowed_tool_names(&request);
        assert_eq!(names.len(), 6);
        assert!(moa_core::types::worker::tool_schema::is_delegation_tool_name("spawn_worker"));
        assert!(moa_core::types::worker::tool_schema::is_delegation_tool_name("wait_worker"));
        assert!(moa_core::types::worker::tool_schema::is_delegation_tool_name("message_worker"));
        assert!(moa_core::types::worker::tool_schema::is_delegation_tool_name("list_workers"));
        assert!(moa_core::types::worker::tool_schema::is_delegation_tool_name("cancel_worker"));
        assert!(
            moa_core::types::worker::tool_schema::is_delegation_tool_name("provide_worker_input")
        );
    }
}
