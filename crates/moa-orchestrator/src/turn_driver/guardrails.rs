//! Pure guardrail response helpers for turn workflows.

use moa_core::{
    types::completion::CompletionContent, types::completion::CompletionResponse,
    types::completion::StopReason, types::guardrails::AgentGuardrailStagePolicy,
};

/// Request for deriving an enforced guardrail block message.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GuardrailBlockMessage<'a> {
    /// Configured stage whose optional message should be honored.
    pub(crate) stage: &'a AgentGuardrailStagePolicy,
    /// Direction-specific fallback message.
    pub(crate) fallback: &'static str,
}

/// Returns the message shown when a guardrail blocks text.
pub(crate) fn block_message(request: GuardrailBlockMessage<'_>) -> String {
    request
        .stage
        .block_message
        .clone()
        .unwrap_or_else(|| request.fallback.to_string())
}

/// Request for replacing an assistant response after output guardrail blocking.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockedOutputResponse<'a> {
    /// Original assistant completion.
    pub(crate) response: &'a CompletionResponse,
    /// Guardrail stage that blocked the response.
    pub(crate) stage: &'a AgentGuardrailStagePolicy,
}

/// Returns the user-visible replacement response for blocked output text.
pub(crate) fn blocked_output_response(request: BlockedOutputResponse<'_>) -> CompletionResponse {
    let text = block_message(GuardrailBlockMessage {
        stage: request.stage,
        fallback: "I can't return that response.",
    });
    let mut visible_response = request.response.clone();
    visible_response.text = text.clone();
    visible_response.content = vec![CompletionContent::Text(text)];
    visible_response.stop_reason = StopReason::EndTurn;
    visible_response.thought_signature = None;
    visible_response
}

#[cfg(test)]
mod tests {
    use moa_core::{
        types::completion::CompletionContent, types::completion::CompletionResponse,
        types::completion::StopReason, types::completion::TokenUsage,
        types::guardrails::AgentGuardrailStagePolicy, types::guardrails::GuardrailMode,
        types::identifiers::ModelId,
    };

    use super::{
        BlockedOutputResponse, GuardrailBlockMessage, block_message, blocked_output_response,
    };

    #[test]
    fn block_message_prefers_stage_message() {
        // Pins: configured guardrail messages remain the source of user-visible block text.
        let stage = AgentGuardrailStagePolicy {
            enabled: true,
            mode: GuardrailMode::Enforce,
            model: None,
            policy_prompt: String::new(),
            block_message: Some("Nope.".to_string()),
        };

        assert_eq!(
            block_message(GuardrailBlockMessage {
                stage: &stage,
                fallback: "fallback",
            }),
            "Nope."
        );
    }

    #[test]
    fn blocked_output_response_clears_hidden_fields() {
        // Pins: blocked output never leaks original text or thought signatures.
        let stage = AgentGuardrailStagePolicy {
            enabled: true,
            mode: GuardrailMode::Enforce,
            model: None,
            policy_prompt: String::new(),
            block_message: None,
        };
        let response = CompletionResponse {
            text: "secret".to_string(),
            content: vec![CompletionContent::Text("secret".to_string())],
            stop_reason: StopReason::ToolUse,
            model: ModelId::new("model"),
            usage: TokenUsage::default(),
            duration_ms: 10,
            thought_signature: Some("hidden".to_string()),
        };

        let blocked = blocked_output_response(BlockedOutputResponse {
            response: &response,
            stage: &stage,
        });
        assert_eq!(blocked.text, "I can't return that response.");
        assert_eq!(
            blocked.content,
            vec![CompletionContent::Text(
                "I can't return that response.".to_string()
            )]
        );
        assert_eq!(blocked.stop_reason, StopReason::EndTurn);
        assert_eq!(blocked.thought_signature, None);
    }
}
