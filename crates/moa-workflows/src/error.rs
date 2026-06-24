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
    /// A workflow definition did not contain a start node.
    #[error("workflow definition must contain exactly one start node")]
    MissingStartNode,
    /// A workflow definition contained more than one start node.
    #[error("workflow definition contains multiple start nodes")]
    MultipleStartNodes,
    /// Workflow execution pointed at a node that is not in the definition.
    #[error("workflow node not found: {node_id}")]
    NodeNotFound {
        /// Missing node identifier.
        node_id: String,
    },
    /// Workflow execution referenced an edge that is not in the definition.
    #[error("workflow edge not found: {edge_id}")]
    EdgeNotFound {
        /// Missing edge identifier.
        edge_id: String,
    },
    /// Workflow state had a current node that was not active.
    #[error("current workflow node `{node_id}` is not active")]
    CurrentNodeNotActive {
        /// Current node identifier missing from the active set.
        node_id: String,
    },
    /// Workflow state had active nodes but no current node.
    #[error("workflow state has active nodes but no current node")]
    MissingCurrentNodeForActiveState,
    /// Workflow state used parallel active nodes before the interpreter supports them.
    #[error("workflow state has {count} active nodes, but parallel interpretation is unsupported")]
    MultipleActiveNodesUnsupported {
        /// Number of active nodes in state.
        count: usize,
    },
    /// Workflow state did not contain the requested blocked node.
    #[error("workflow node `{node_id}` is not blocked")]
    BlockedNodeNotFound {
        /// Node that was expected to be blocked.
        node_id: String,
    },
    /// No outgoing edge matched the current workflow state.
    #[error("workflow node `{node_id}` has no matching outgoing edge")]
    NoMatchingOutgoingEdge {
        /// Node that could not choose a branch.
        node_id: String,
    },
    /// More than one outgoing edge matched the current workflow state.
    #[error("workflow node `{node_id}` matched {matched_count} outgoing edges")]
    AmbiguousOutgoingEdges {
        /// Node that selected too many branches.
        node_id: String,
        /// Number of matching outgoing edges.
        matched_count: usize,
    },
    /// Expression conditions are intentionally unsupported in the pure interpreter.
    #[error("unsupported workflow condition expression `{language}`: {expression}")]
    UnsupportedConditionExpression {
        /// Expression language identifier.
        language: String,
        /// Expression source text.
        expression: String,
    },
    /// A loop back-edge exceeded the configured iteration guard.
    #[error(
        "workflow loop `{edge_id}` exceeded max iterations: attempted {attempted_iterations}, max {max_iterations}"
    )]
    LoopIterationLimitExceeded {
        /// Edge identifier used as the loop counter key.
        edge_id: String,
        /// Iteration count attempted by the transition.
        attempted_iterations: u32,
        /// Maximum iterations allowed by state.
        max_iterations: u32,
    },
    /// A parallel node exceeded its configured branch fan-out.
    #[error(
        "workflow parallel node `{node_id}` attempted {branch_count} branches, max {max_branches}"
    )]
    ParallelFanOutExceeded {
        /// Parallel node that attempted too many branches.
        node_id: String,
        /// Number of branches selected by graph edges.
        branch_count: usize,
        /// Maximum branches allowed by state.
        max_branches: u32,
    },
    /// A required branch failed before a join could complete.
    #[error(
        "workflow join `{join_node_id}` cannot continue because branches failed: {failed_node_ids:?}"
    )]
    ParallelBranchFailed {
        /// Join node blocked by failed branches.
        join_node_id: String,
        /// Required branch node IDs that failed.
        failed_node_ids: Vec<String>,
    },
    /// The interpreter reached a workflow node kind that this task does not support yet.
    #[error("workflow node `{node_id}` has unsupported kind `{kind}`")]
    UnsupportedNodeKind {
        /// Node with unsupported runtime behavior.
        node_id: String,
        /// Unsupported node kind.
        kind: String,
    },
    /// Artifact storage returned an error.
    #[error(transparent)]
    Artifact(#[from] moa_core::MoaError),
}
