//! Consistent MCP tool success and execution-error results.

use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock, MetaObject};
use serde::Serialize;
use serde_json::json;

use super::command::McpCommandError;

/// Build a successful tool result with concise text and exact structured content.
pub(crate) fn success(summary: impl Into<String>, response: &impl Serialize) -> CallToolResult {
    match serde_json::to_value(response) {
        Ok(value) => {
            let summary = summary.into();
            let mut result = CallToolResult::structured(json!({
                "summary": summary,
                "data": value,
            }));
            result.content = vec![ContentBlock::text(summary)];
            result
        }
        Err(error) => execution_error(format!("failed to encode tool result: {error}")),
    }
}

/// Map a typed Restate command response or expected execution failure into a tool result.
pub(crate) fn command_result(
    summary: impl Into<String>,
    result: Result<impl Serialize, McpCommandError>,
) -> CallToolResult {
    match result {
        Ok(response) => success(summary, &response),
        Err(error) => execution_error(error.to_string()),
    }
}

/// Build a caller-visible tool execution error rather than a JSON-RPC protocol error.
pub(crate) fn execution_error(message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    let mut result = CallToolResult::structured_error(json!({ "error": message }));
    result.content = vec![ContentBlock::text(message)];
    result
}

/// Rebuild any errored result that lacks the documented structured `{error}`
/// envelope, such as rmcp's own tool-argument deserialization failures.
pub(crate) fn normalize(result: CallToolResult) -> CallToolResult {
    if result.is_error == Some(true) && result.structured_content.is_none() {
        let message = result
            .content
            .iter()
            .find_map(|block| block.as_text().map(|text| text.text.clone()))
            .unwrap_or_else(|| "tool call failed".to_string());
        return execution_error(message);
    }
    result
}

/// Normalize a completed tool response and attach this server's per-response identity.
pub(crate) fn normalize_response(
    response: CallToolResponse,
    server_meta: MetaObject,
) -> CallToolResponse {
    match response {
        CallToolResponse::Complete(result) => {
            let mut result = normalize(result);
            let mut meta = result.meta.take().unwrap_or_default();
            for (key, value) in server_meta.0 {
                meta.insert(key, value);
            }
            result.meta = Some(meta);
            CallToolResponse::Complete(result)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolResult, ContentBlock};
    use serde_json::json;

    use super::{normalize, success};

    #[test]
    fn normalize_wraps_bare_errors_and_preserves_conforming_results_offline() {
        // Pins: every errored tool result reaching the client carries the
        // documented {error} structuredContent, including rmcp-generated
        // argument-deserialization failures that bypass our handlers.
        let bare = CallToolResult::error(vec![ContentBlock::text(
            "failed to deserialize parameters: invalid type",
        )]);
        let normalized = normalize(bare);
        assert_eq!(
            normalized.structured_content,
            Some(json!({ "error": "failed to deserialize parameters: invalid type" }))
        );
        assert_eq!(normalized.is_error, Some(true));

        let conforming = success("ok", &json!({ "value": 1 }));
        let untouched = normalize(conforming.clone());
        assert_eq!(untouched.structured_content, conforming.structured_content);
        assert_eq!(untouched.is_error, Some(false));
    }

    #[test]
    fn success_returns_the_documented_summary_and_data_envelope_offline() {
        // Pins: successful MCP calls have one stable structuredContent shape even
        // when the owning service's typed response changes.
        let result = success("Validated artifact", &json!({ "valid": true }));

        assert_eq!(
            result.structured_content,
            Some(json!({
                "summary": "Validated artifact",
                "data": { "valid": true }
            }))
        );
        assert_eq!(result.is_error, Some(false));
    }
}
