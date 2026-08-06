//! Deterministic initial-plan compilation and restricted amendment validation.

mod amendment;
mod estimate;
mod validation;

use amendment::*;
use estimate::*;
use validation::*;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_artifacts::{
    execution_plan::{
        CompletionCheckKind, ExecutionBudgetLimit, ExecutionCondition, ExecutionGoalContract,
        ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionReducer, MapTask,
        PlanAmendment, PlanAmendmentOperation,
    },
    reference::ArtifactRef,
    validation::{validate_execution_goal_contract, validate_execution_plan_definition},
};
use moa_config::ExecutionConfig;
use moa_core::canonical_json::canonical_json_bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Error,
    bindings::extract_map_key,
    budget::estimate_fits_limit,
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapability, ExecutionCapabilityCatalog,
        ExecutionEstimate, ExecutionHash, canonical_sort_key, catalog_hash, plan_hash,
    },
    schema::{validate_instance, validate_schema},
    state::{ExecutionNodeStatus, ExecutionProjection, ExecutionTaskStatus},
};

/// Complete input to deterministic initial execution compilation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileExecutionRequest {
    /// Immutable user-derived goal contract.
    pub goal: ExecutionGoalContract,
    /// Candidate execution plan.
    pub plan: ExecutionPlanDefinition,
    /// Concrete run input validated against the plan input schema.
    pub run_input: Value,
    /// Immutable tenant- and policy-filtered capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Immutable exact capability and skill allowlist.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Approved run-level resource envelope.
    pub approved_budget: ExecutionBudgetLimit,
    /// Tenant-independent execution defaults and per-turn estimates.
    pub config: ExecutionConfig,
    /// Deterministic compile time used for deadline validation.
    pub now: DateTime<Utc>,
}

/// Complete input to restricted plan-amendment validation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateAmendmentRequest {
    /// Immutable goal contract that amendments cannot replace.
    pub goal: ExecutionGoalContract,
    /// Active canonical plan snapshot.
    pub active_plan: CanonicalExecutionPlan,
    /// Restricted pending/downstream patch.
    pub amendment: PlanAmendment,
    /// Current durable run projection.
    pub projection: ExecutionProjection,
    /// Current immutable capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Original immutable authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Resource envelope remaining before the amendment is accepted.
    pub remaining_budget: ExecutionBudgetLimit,
    /// Execution defaults and per-turn estimates.
    pub config: ExecutionConfig,
    /// Deterministic validation time.
    pub now: DateTime<Utc>,
}

/// Outcome of initial compilation with inspectable ordered validation issues.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileExecutionOutcome {
    /// Canonical goal and plan when no validation error exists.
    pub compiled: Option<CompiledExecution>,
    /// Ordered validation report.
    pub report: ExecutionValidationReport,
}

/// Outcome of amendment validation with inspectable ordered issues.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentValidationOutcome {
    /// New canonical plan snapshot when the amendment is valid.
    pub plan: Option<CanonicalExecutionPlan>,
    /// Worst-case estimate of only work still executable after the amendment.
    pub remaining_estimate: Option<ExecutionEstimate>,
    /// Ordered validation report.
    pub report: ExecutionValidationReport,
}

/// Validated immutable goal and canonical execution plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledExecution {
    /// Validated immutable goal contract.
    pub goal: ExecutionGoalContract,
    /// Canonical immutable plan snapshot.
    pub plan: CanonicalExecutionPlan,
}

/// Canonical compiled plan and its immutable hashes and estimate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalExecutionPlan {
    /// Exact validated plan definition.
    pub definition: ExecutionPlanDefinition,
    /// Domain-separated canonical plan hash.
    pub plan_hash: ExecutionHash,
    /// Pinned capability-catalog hash.
    pub catalog_hash: ExecutionHash,
    /// Exact worst-case plan estimate, including verifier tasks.
    pub estimate: ExecutionEstimate,
    /// Compiler report stored with the immutable snapshot.
    pub report: ExecutionValidationReport,
}

/// Ordered compiler validation report.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionValidationReport {
    /// Validation issues in deterministic discovery order.
    pub issues: Vec<ExecutionValidationIssue>,
}

impl ExecutionValidationReport {
    /// Returns whether at least one blocking error exists.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == ExecutionValidationSeverity::Error)
    }

    pub(crate) fn error(
        &mut self,
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.issues.push(ExecutionValidationIssue {
            severity: ExecutionValidationSeverity::Error,
            code: code.into(),
            path: path.into(),
            message: message.into(),
        });
    }
}

/// One deterministic compiler validation issue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionValidationIssue {
    /// Blocking or informational severity.
    pub severity: ExecutionValidationSeverity,
    /// Stable machine-readable issue code.
    pub code: String,
    /// JSON-ish location of the invalid value.
    pub path: String,
    /// Human-readable validation failure.
    pub message: String,
}

/// Compiler validation severity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionValidationSeverity {
    /// Blocking validation issue.
    Error,
    /// Non-blocking validation issue persisted for audit.
    Warning,
}

/// Deterministically validates, estimates, canonicalizes, and hashes an initial plan.
#[must_use]
pub fn compile(request: CompileExecutionRequest) -> CompileExecutionOutcome {
    let mut report = ExecutionValidationReport::default();
    append_artifact_reports(&request.goal, &request.plan, &mut report);
    validate_goal_plan_links(&request.goal, &request.plan, &mut report);
    validate_catalog(&request.catalog, &mut report);
    validate_authorization(&request.authorization, &mut report);
    validate_schemas(&request.goal, &request.plan, &mut report);
    validate_declared_reference_paths(&request.goal, &request.plan, &mut report);
    if let Err(error) = validate_instance(
        &request.plan.input_schema,
        &request.run_input,
        "plan.input_schema",
    ) {
        append_error(&mut report, "invalid_run_input", "run_input", error);
    }
    validate_plan_references(
        &request.plan,
        &request.catalog,
        &request.authorization,
        &mut report,
    );

    if request
        .approved_budget
        .deadline_at
        .is_some_and(|deadline| request.now > deadline)
    {
        report.error(
            "deadline_exceeded",
            "approved_budget.deadline_at",
            "approved execution deadline has already elapsed",
        );
    }

    let estimate = estimate_plan(
        &request.goal,
        &request.plan,
        &request.catalog,
        &request.config,
        &mut report,
    );
    if let Some(estimate) = estimate
        && let Err(error) = estimate_fits_limit(estimate, &request.approved_budget)
    {
        append_error(
            &mut report,
            "approved_budget_exceeded",
            "approved_budget",
            error,
        );
    }

    let hash = match plan_hash(&request.plan) {
        Ok(hash) => Some(hash),
        Err(error) => {
            append_error(&mut report, "plan_hash_failed", "plan", error);
            None
        }
    };

    if report.has_errors() {
        return CompileExecutionOutcome {
            compiled: None,
            report,
        };
    }

    let Some(estimate) = estimate else {
        report.error(
            "estimate_missing",
            "plan",
            "validated plan did not produce a worst-case estimate",
        );
        return CompileExecutionOutcome {
            compiled: None,
            report,
        };
    };
    let Some(hash) = hash else {
        report.error(
            "plan_hash_missing",
            "plan",
            "validated plan did not produce a canonical hash",
        );
        return CompileExecutionOutcome {
            compiled: None,
            report,
        };
    };

    let plan = CanonicalExecutionPlan {
        definition: request.plan,
        plan_hash: hash,
        catalog_hash: request.catalog.catalog_hash,
        estimate,
        report: report.clone(),
    };
    CompileExecutionOutcome {
        compiled: Some(CompiledExecution {
            goal: request.goal,
            plan,
        }),
        report,
    }
}

/// Validates and canonicalizes one restricted amendment over pending work.
#[must_use]
pub fn validate_amendment(request: ValidateAmendmentRequest) -> AmendmentValidationOutcome {
    let mut report = ExecutionValidationReport::default();
    if request.amendment.base_plan_revision != request.projection.plan_revision {
        report.error(
            "stale_plan_revision",
            "amendment.base_plan_revision",
            "amendment base revision does not match the active projection",
        );
    }
    if request.amendment.reason.trim().is_empty() {
        report.error(
            "empty_amendment_reason",
            "amendment.reason",
            "amendment reason must not be empty",
        );
    }

    validate_catalog(&request.catalog, &mut report);
    validate_authorization(&request.authorization, &mut report);
    if request.catalog.catalog_hash != request.active_plan.catalog_hash {
        report.error(
            "catalog_hash_changed",
            "catalog.catalog_hash",
            "amendment must use the capability catalog pinned by the active plan",
        );
    }

    let mut definition = request.active_plan.definition.clone();
    apply_amendment(
        &request.amendment,
        &request.projection,
        &request.active_plan.definition,
        &mut definition,
        &mut report,
    );
    validate_amendment_reference_narrowing(
        &request.active_plan.definition,
        &definition,
        &mut report,
    );

    append_artifact_reports(&request.goal, &definition, &mut report);
    validate_goal_plan_links(&request.goal, &definition, &mut report);
    validate_schemas(&request.goal, &definition, &mut report);
    validate_declared_reference_paths(&request.goal, &definition, &mut report);
    validate_plan_references(
        &definition,
        &request.catalog,
        &request.authorization,
        &mut report,
    );

    if request
        .remaining_budget
        .deadline_at
        .is_some_and(|deadline| request.now > deadline)
    {
        report.error(
            "deadline_exceeded",
            "remaining_budget.deadline_at",
            "execution deadline has already elapsed",
        );
    }

    let full_estimate = estimate_plan(
        &request.goal,
        &definition,
        &request.catalog,
        &request.config,
        &mut report,
    );
    let remaining_estimate = estimate_remaining_plan(
        &request.goal,
        &definition,
        &request.projection,
        &request.catalog,
        &request.config,
        &mut report,
    );
    if let Some(estimate) = remaining_estimate
        && let Err(error) = estimate_fits_limit(estimate, &request.remaining_budget)
    {
        append_error(
            &mut report,
            "remaining_budget_exceeded",
            "remaining_budget",
            error,
        );
    }

    let hash = match plan_hash(&definition) {
        Ok(hash) => Some(hash),
        Err(error) => {
            append_error(&mut report, "plan_hash_failed", "plan", error);
            None
        }
    };

    if report.has_errors() {
        return AmendmentValidationOutcome {
            plan: None,
            remaining_estimate,
            report,
        };
    }

    let (Some(estimate), Some(hash)) = (full_estimate, hash) else {
        report.error(
            "canonical_plan_missing",
            "plan",
            "validated amendment did not produce a canonical plan",
        );
        return AmendmentValidationOutcome {
            plan: None,
            remaining_estimate,
            report,
        };
    };

    AmendmentValidationOutcome {
        plan: Some(CanonicalExecutionPlan {
            definition,
            plan_hash: hash,
            catalog_hash: request.catalog.catalog_hash,
            estimate,
            report: report.clone(),
        }),
        remaining_estimate,
        report,
    }
}

#[cfg(test)]
mod tests;
