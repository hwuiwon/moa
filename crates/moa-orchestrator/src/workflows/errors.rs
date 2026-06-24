//! Shared workflow error conversion for Restate handlers.

use moa_workflows::error::WorkflowError;
use restate_sdk::prelude::*;

/// Converts workflow-domain errors into Restate handler errors.
pub(crate) fn workflow_handler_error(error: WorkflowError) -> HandlerError {
    match error {
        WorkflowError::InvalidReference { .. } | WorkflowError::WrongReferenceKind => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        WorkflowError::MissingStartNode
        | WorkflowError::MultipleStartNodes
        | WorkflowError::NodeNotFound { .. }
        | WorkflowError::EdgeNotFound { .. }
        | WorkflowError::CurrentNodeNotActive { .. }
        | WorkflowError::MissingCurrentNodeForActiveState
        | WorkflowError::MultipleActiveNodesUnsupported { .. }
        | WorkflowError::BlockedNodeNotFound { .. }
        | WorkflowError::NoMatchingOutgoingEdge { .. }
        | WorkflowError::AmbiguousOutgoingEdges { .. }
        | WorkflowError::UnsupportedConditionExpression { .. }
        | WorkflowError::LoopIterationLimitExceeded { .. }
        | WorkflowError::ParallelFanOutExceeded { .. }
        | WorkflowError::ParallelBranchFailed { .. }
        | WorkflowError::UnsupportedNodeKind { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        WorkflowError::WorkflowNotFound { .. } => {
            TerminalError::new_with_code(404, error.to_string()).into()
        }
        WorkflowError::Artifact(source) => {
            if source.is_fatal() {
                TerminalError::new(source.to_string()).into()
            } else {
                HandlerError::from(source)
            }
        }
    }
}
