//! Typed planning outcomes consumed by orchestration and durable audit persistence.

use moa_artifacts::execution_plan::ExecutionBudgetLimit;
use moa_core::types::execution_planning::{
    ExecutionPlanningAuditEnvelopeV1, ExecutionSourceProvenanceV1,
};
use moa_execution::compiler::CompiledExecution;
use serde_json::Value;

/// Successful compiler output ready for immutable execution admission.
#[derive(Clone, Debug)]
pub struct AdmittedExecutionPlan {
    /// Validated immutable goal and canonical plan.
    pub compiled: CompiledExecution,
    /// Canonical structured input used by the plan.
    pub run_input: Value,
    /// Closed source provenance persisted on the execution run.
    pub source_provenance: ExecutionSourceProvenanceV1,
    /// Approved budget copied from the immutable planning context.
    pub approved_budget: ExecutionBudgetLimit,
}

/// Closed result of initial execution planning or exact template instantiation.
#[derive(Clone, Debug)]
pub enum ExecutionPlanningResultKind {
    /// A canonical plan is ready for Execution/start.
    Ready(Box<AdmittedExecutionPlan>),
    /// Structured input or user clarification is required.
    NeedsInput { message: String },
    /// The plan cannot be served without broadening the frozen contract.
    Unsupported { message: String },
}

/// Planning result plus every provider/compiler audit record produced by the operation.
#[derive(Clone, Debug)]
pub struct ExecutionPlanningResult {
    /// Closed planning result.
    pub kind: ExecutionPlanningResultKind,
    /// Ordered immutable planner and compiler audit envelopes.
    pub audits: Vec<ExecutionPlanningAuditEnvelopeV1>,
}

/// Closed result of amendment generation and validation.
#[derive(Clone, Debug)]
pub enum ExecutionAmendmentPlanningResultKind {
    /// Compiler-validated replacement plan ready for revision-fenced application.
    Ready {
        /// Validated replacement canonical plan.
        plan: Box<moa_execution::compiler::CanonicalExecutionPlan>,
        /// Canonical amendment candidate applied to produce the plan.
        amendment: moa_artifacts::execution_plan::PlanAmendment,
        /// Candidate identity used by repository idempotency fences.
        candidate_hash: String,
    },
    /// Planning cannot proceed without caller input.
    NeedsInput { message: String },
    /// Planning cannot produce an authorized amendment.
    Unsupported { message: String },
}

/// Amendment result plus ordered planner/compiler audit records.
#[derive(Clone, Debug)]
pub struct ExecutionAmendmentPlanningResult {
    /// Closed amendment outcome.
    pub kind: ExecutionAmendmentPlanningResultKind,
    /// Ordered immutable audit envelopes.
    pub audits: Vec<ExecutionPlanningAuditEnvelopeV1>,
}
