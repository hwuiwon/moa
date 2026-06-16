//! Error types for artifact-backed workflow runtime operations.

/// Result type returned by workflow runtime APIs.
pub type Result<T> = std::result::Result<T, WorkflowError>;

/// Errors returned by workflow runtime APIs.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// The supplied workflow reference could not be parsed.
    #[error("invalid workflow reference `{reference}`: {message}")]
    InvalidReference {
        /// Rejected reference string.
        reference: String,
        /// Rejection reason.
        message: String,
    },
    /// The supplied reference did not point to a workflow artifact.
    #[error("workflow_ref must use workflow://")]
    WrongReferenceKind,
    /// The requested workflow artifact was not visible as a published revision.
    #[error("published workflow artifact not found: {workflow_ref}")]
    WorkflowNotFound {
        /// Workflow reference that did not resolve.
        workflow_ref: String,
    },
    /// Artifact storage returned an error.
    #[error(transparent)]
    Artifact(#[from] moa_core::MoaError),
}
