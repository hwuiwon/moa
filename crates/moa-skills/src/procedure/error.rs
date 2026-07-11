//! Error types for skill-backed procedure runtime operations.

/// Result type returned by procedure runtime APIs.
pub type Result<T> = std::result::Result<T, ProcedureError>;

/// Errors returned by procedure runtime APIs.
#[derive(Debug, thiserror::Error)]
pub enum ProcedureError {
    /// The supplied procedure reference could not be parsed.
    #[error("invalid procedure reference `{reference}`: {message}")]
    InvalidReference {
        /// Rejected reference string.
        reference: String,
        /// Rejection reason.
        message: String,
    },
    /// The supplied reference did not point to a skill artifact.
    #[error("procedure_ref must use skill://")]
    WrongReferenceKind,
    /// The requested skill artifact was not visible as a published revision.
    #[error("published skill artifact not found: {procedure_ref}")]
    ProcedureNotFound {
        /// Procedure reference that did not resolve.
        procedure_ref: String,
    },
    /// The referenced skill revision does not carry a procedure graph.
    #[error("skill artifact `{procedure_ref}` does not define a procedure")]
    SkillHasNoProcedure {
        /// Skill reference that resolved but lacked a procedure.
        procedure_ref: String,
    },
    /// The caller-supplied input did not satisfy the procedure's `input_schema`.
    ///
    /// `missing` lists required fields that were absent from the input, and
    /// `invalid` lists provided fields whose value type did not match the
    /// declared schema. Callers must collect the listed fields before retrying.
    #[error(
        "procedure input does not satisfy input_schema: missing {missing:?}, invalid types {invalid:?}"
    )]
    MissingRequiredInputs {
        /// Required fields absent from the supplied input.
        missing: Vec<String>,
        /// Provided fields whose value type did not match the schema.
        invalid: Vec<String>,
    },
    /// A procedure definition did not contain a start node.
    #[error("procedure definition must contain exactly one start node")]
    MissingStartNode,
    /// A procedure definition contained more than one start node.
    #[error("procedure definition contains multiple start nodes")]
    MultipleStartNodes,
    /// Procedure execution pointed at a node that is not in the definition.
    #[error("procedure node not found: {node_id}")]
    NodeNotFound {
        /// Missing node identifier.
        node_id: String,
    },
    /// Procedure execution referenced an edge that is not in the definition.
    #[error("procedure edge not found: {edge_id}")]
    EdgeNotFound {
        /// Missing edge identifier.
        edge_id: String,
    },
    /// Procedure state had a current node that was not active.
    #[error("current procedure node `{node_id}` is not active")]
    CurrentNodeNotActive {
        /// Current node identifier missing from the active set.
        node_id: String,
    },
    /// Procedure state had active nodes but no current node.
    #[error("procedure state has active nodes but no current node")]
    MissingCurrentNodeForActiveState,
    /// Procedure state used parallel active nodes before the interpreter supports them.
    #[error("procedure state has {count} active nodes, but parallel interpretation is unsupported")]
    MultipleActiveNodesUnsupported {
        /// Number of active nodes in state.
        count: usize,
    },
    /// Procedure state did not contain the requested blocked node.
    #[error("procedure node `{node_id}` is not blocked")]
    BlockedNodeNotFound {
        /// Node that was expected to be blocked.
        node_id: String,
    },
    /// No outgoing edge matched the current procedure state.
    #[error("procedure node `{node_id}` has no matching outgoing edge")]
    NoMatchingOutgoingEdge {
        /// Node that could not choose a branch.
        node_id: String,
    },
    /// More than one outgoing edge matched the current procedure state.
    #[error("procedure node `{node_id}` matched {matched_count} outgoing edges")]
    AmbiguousOutgoingEdges {
        /// Node that selected too many branches.
        node_id: String,
        /// Number of matching outgoing edges.
        matched_count: usize,
    },
    /// A loop back-edge exceeded the configured iteration guard.
    #[error(
        "procedure loop `{edge_id}` exceeded max iterations: attempted {attempted_iterations}, max {max_iterations}"
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
        "procedure parallel node `{node_id}` attempted {branch_count} branches, max {max_branches}"
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
        "procedure join `{join_node_id}` cannot continue because branches failed: {failed_node_ids:?}"
    )]
    ParallelBranchFailed {
        /// Join node blocked by failed branches.
        join_node_id: String,
        /// Required branch node IDs that failed.
        failed_node_ids: Vec<String>,
    },
    /// The interpreter reached a procedure node kind that this task does not support yet.
    #[error("procedure node `{node_id}` has unsupported kind `{kind}`")]
    UnsupportedNodeKind {
        /// Node with unsupported runtime behavior.
        node_id: String,
        /// Unsupported node kind.
        kind: String,
    },
    /// Artifact storage returned an error.
    #[error(transparent)]
    Artifact(#[from] moa_core::error::MoaError),
}
