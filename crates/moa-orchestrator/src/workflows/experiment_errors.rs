//! Shared error conversions for the experiment-run and trial-run workflows.

use moa_experiments::plan::PlanExpansionError;
use restate_sdk::prelude::*;

use crate::workflows::errors::{bad_request, handler_error_message};

/// Converts a plan-expansion error into a terminal `400` handler error.
pub(crate) fn plan_expansion_error_to_handler_error(error: PlanExpansionError) -> HandlerError {
    bad_request(error.to_string())
}

/// Wraps a handler error as a non-retryable terminal failure, preserving its message.
pub(crate) fn non_retryable_handler_error(error: HandlerError) -> HandlerError {
    TerminalError::new(handler_error_message(&error)).into()
}
