//! Shared workflow error conversion for Restate handlers.

use moa_core::MoaError;
use moa_skills::procedure::error::ProcedureError;
use restate_sdk::prelude::*;

/// Converts a [`MoaError`] into a Restate handler error.
///
/// Fatal (non-retryable) errors become terminal failures; transient errors are
/// returned as retryable handler errors so Restate can re-invoke the handler.
pub(crate) fn moa_error_to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

/// Converts a [`MoaError`] into a Restate handler error with HTTP-style codes.
///
/// Validation, serialization, and UUID errors map to `400`; unsupported and
/// not-implemented errors map to `501`; remaining fatal errors become terminal
/// failures and transient errors stay retryable.
pub(crate) fn moa_error_to_status_handler_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::ValidationError(_) | MoaError::SerializationError(_) | MoaError::Uuid(_) => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        MoaError::Unsupported(_) | MoaError::NotImplemented(_) => {
            TerminalError::new_with_code(501, error.to_string()).into()
        }
        other if other.is_fatal() => TerminalError::new(other.to_string()).into(),
        other => HandlerError::from(other),
    }
}

/// Builds a terminal `400` handler error from a message.
pub(crate) fn bad_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

/// Extracts the display message from a Restate handler error.
pub(crate) fn handler_error_message(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
}

/// Converts procedure-domain errors into Restate handler errors.
pub(crate) fn procedure_handler_error(error: ProcedureError) -> HandlerError {
    match error {
        ProcedureError::MissingRequiredInputs { missing, invalid } => {
            TerminalError::new_with_code(400, missing_inputs_message(&missing, &invalid)).into()
        }
        ProcedureError::InvalidReference { .. }
        | ProcedureError::WrongReferenceKind
        | ProcedureError::SkillHasNoProcedure { .. }
        | ProcedureError::MissingStartNode
        | ProcedureError::MultipleStartNodes
        | ProcedureError::NodeNotFound { .. }
        | ProcedureError::EdgeNotFound { .. }
        | ProcedureError::CurrentNodeNotActive { .. }
        | ProcedureError::MissingCurrentNodeForActiveState
        | ProcedureError::MultipleActiveNodesUnsupported { .. }
        | ProcedureError::BlockedNodeNotFound { .. }
        | ProcedureError::NoMatchingOutgoingEdge { .. }
        | ProcedureError::AmbiguousOutgoingEdges { .. }
        | ProcedureError::UnsupportedConditionExpression { .. }
        | ProcedureError::LoopIterationLimitExceeded { .. }
        | ProcedureError::ParallelFanOutExceeded { .. }
        | ProcedureError::ParallelBranchFailed { .. }
        | ProcedureError::UnsupportedNodeKind { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        ProcedureError::ProcedureNotFound { .. } => {
            TerminalError::new_with_code(404, error.to_string()).into()
        }
        ProcedureError::Artifact(source) => moa_error_to_handler_error(source),
    }
}

/// Builds the machine-readable message for a missing-inputs terminal error.
///
/// The message embeds a compact JSON payload so any caller (a UI, or the agent
/// via the `run_procedure` tool) can parse exactly which fields to collect.
pub(crate) fn missing_inputs_message(missing: &[String], invalid: &[String]) -> String {
    let payload = serde_json::json!({
        "error": "missing_required_inputs",
        "missing_inputs": missing,
        "invalid_inputs": invalid,
    });
    format!(
        "procedure input does not satisfy input_schema: {}",
        serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string())
    )
}

#[cfg(test)]
mod tests {
    use moa_skills::procedure::error::ProcedureError;

    use super::{handler_error_message, missing_inputs_message, procedure_handler_error};

    #[test]
    fn missing_inputs_message_embeds_parseable_field_lists() {
        // Pins: the terminal message carries a compact JSON payload naming the
        // exact fields a caller must collect before retrying.
        let message = missing_inputs_message(&["order_id".to_string()], &["quantity".to_string()]);
        let json_start = message.find('{').expect("message embeds json payload");
        let payload: serde_json::Value =
            serde_json::from_str(&message[json_start..]).expect("payload parses");

        assert_eq!(payload["error"], "missing_required_inputs");
        assert_eq!(payload["missing_inputs"], serde_json::json!(["order_id"]));
        assert_eq!(payload["invalid_inputs"], serde_json::json!(["quantity"]));
    }

    #[test]
    fn missing_required_inputs_renders_machine_readable_message() {
        // Pins: a schema violation maps to a message an agent/UI can parse to learn
        // exactly which fields to collect before retrying.
        let handler_error = procedure_handler_error(ProcedureError::MissingRequiredInputs {
            missing: vec!["reason".to_string()],
            invalid: vec!["quantity".to_string()],
        });
        let message = handler_error_message(&handler_error);

        assert!(message.contains("missing_inputs"));
        assert!(message.contains("reason"));
        assert!(message.contains("invalid_inputs"));
        assert!(message.contains("quantity"));
    }

    #[test]
    fn skill_without_procedure_renders_no_procedure_message() {
        // Pins: an agent-mediated skill (no procedure graph) cannot start a run.
        let handler_error = procedure_handler_error(ProcedureError::SkillHasNoProcedure {
            procedure_ref: "skill://greeter".to_string(),
        });
        let message = handler_error_message(&handler_error);
        assert!(message.contains("does not define a procedure"));
    }
}
