//! Anthropic response normalization helpers.

use super::*;

pub(super) fn stop_reason_from_anthropic(stop_reason: String) -> StopReason {
    match stop_reason.as_str() {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "cancelled" => StopReason::Cancelled,
        other => StopReason::Other(other.to_string()),
    }
}
