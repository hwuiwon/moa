//! Pure compilation and interpretation for durable MOA execution runs.

/// Restricted execution binding resolution.
pub mod bindings;
/// Integer-only run budget accounting.
pub mod budget;
/// Capability catalogs, estimates, and canonical hashes.
pub mod capability;
/// Initial-plan compilation and restricted amendment validation.
pub mod compiler;
/// Deterministic completion evaluation.
pub mod completion;
/// Crate error contract.
pub mod error;
/// Pure bounded logical-task materialization.
pub mod interpreter;
/// Deterministic replan-stop evaluation.
pub mod replan;
/// Scoped PostgreSQL execution-run persistence.
pub mod repository;
/// Draft 2020-12 schema validation.
pub mod schema;
/// Public execution projection and task state.
pub mod state;
/// Public execution service and internal durable-workflow wire contracts.
pub mod wire;

pub use capability::{
    CapabilitiesListRequest, CapabilitiesListResponse, CapabilityCatalogDiagnostic,
    CapabilityCatalogDiagnosticCode, CapabilityPolicyContext, CapabilitySource,
    ExecutionAuthorizationEnvelope, ExecutionCapability, ExecutionCapabilityCatalog,
    ExecutionClass, ExecutionEstimate, ExecutionHash,
};
pub use compiler::{
    AmendmentValidationOutcome, CanonicalExecutionPlan, CompileExecutionOutcome,
    CompileExecutionRequest, CompiledExecution, ExecutionValidationIssue,
    ExecutionValidationReport, ExecutionValidationSeverity, ValidateAmendmentRequest, compile,
    validate_amendment,
};
pub use completion::{
    CompletionCheckResult, CompletionEvaluation, CompletionEvaluationRequest, CompletionStatus,
    evaluate_completion, execution_terminal_reason,
};
pub use error::Error;
pub use interpreter::{
    NodeMaterializationPage, ReduceMaterializationCursor, ReduceMaterializationPageInput,
    ScheduleRequest, materialize_node_page,
};
pub use replan::{ReplanDecision, ReplanEvaluationRequest, ReplanStopReason, evaluate_replan_stop};
pub use state::{ExecutionSourceKind, ExecutionTerminalReason};

/// Result type returned by fallible pure execution operations.
pub type Result<T> = std::result::Result<T, Error>;
