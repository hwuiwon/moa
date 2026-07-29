//! Shared workflow error conversion for Restate handlers.

use moa_core::error::MoaError;
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
/// Validation, serialization, and UUID errors map to `400`; unsupported errors
/// map to `501`; remaining fatal errors become terminal failures and transient
/// errors stay retryable.
pub(crate) fn moa_error_to_status_handler_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::ValidationError(_) | MoaError::SerializationError(_) | MoaError::Uuid(_) => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        MoaError::Unsupported(_) => TerminalError::new_with_code(501, error.to_string()).into(),
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
