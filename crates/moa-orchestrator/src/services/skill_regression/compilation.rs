//! Strict execution-template compilation and audit construction for skill regression.

use std::time::Instant;

use chrono::Utc;
use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionGoalContract, ExecutionPlanDefinition, ExecutionPlanTemplate,
};
use moa_config::MoaConfig;
use moa_core::{
    canonical_json::canonical_json_bytes,
    error::{MoaError, Result},
    types::{
        execution_planning::{
            ExecutionAuditViolation, ExecutionCompileOutcome, ExecutionCompileSource,
            ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload, bounded_audit_report,
            execution_planning_hash,
        },
        experience::LearningCandidate,
        identifiers::TenantId,
    },
};
use moa_eval_core::TestSuite;
use moa_execution::{
    CompileExecutionOutcome, CompileExecutionRequest, ExecutionAuthorizationEnvelope,
    ExecutionCapabilityCatalog, ExecutionValidationReport, ExecutionValidationSeverity, compile,
    schema::validate_instance,
};
use serde::Serialize;
use serde_json::{Value, json};

use super::suite::RegressionExecutionInput;

/// Inputs for compiling a template-bearing skill revision at the regression boundary.
pub(super) struct SkillTemplateCompileRequest<'a> {
    /// Runtime execution limits used by the shared compiler.
    pub(super) config: &'a MoaConfig,
    /// Tenant that owns the proposed skill revision.
    pub(super) tenant_id: TenantId,
    /// Proposed skill name used to instantiate the execution goal.
    pub(super) skill_name: &'a str,
    /// Skill-level structured input schema.
    pub(super) skill_input_schema: &'a Value,
    /// Execution-plan template carried by the draft revision.
    pub(super) template: &'a ExecutionPlanTemplate,
    /// Explicit structured input resolved from the regression suite.
    pub(super) run_input: &'a RegressionExecutionInput,
    /// Governed capability catalog available to this review.
    pub(super) catalog: &'a ExecutionCapabilityCatalog,
    /// Exact capability authorization envelope for this review.
    pub(super) authorization: &'a ExecutionAuthorizationEnvelope,
    /// Stable idempotency key for the compile audit.
    pub(super) operation_key: &'a str,
}

/// Strict compiler result and the audit envelope that records it.
pub(super) struct SkillTemplateCompile {
    /// Sessionless compile audit persisted before regression execution.
    pub(super) audit: ExecutionPlanningAuditEnvelope,
    /// Whether compilation produced a valid, authorized plan.
    pub(super) accepted: bool,
}

#[derive(Serialize)]
struct InitialCompileCandidate<'a> {
    kind: &'static str,
    schema_version: u8,
    source: ExecutionCompileSource,
    goal: &'a ExecutionGoalContract,
    plan: &'a ExecutionPlanDefinition,
    run_input: &'a Value,
}

/// Compiles a skill execution-plan template and builds its strict audit envelope.
pub(super) fn compile_skill_execution_template(
    request: SkillTemplateCompileRequest<'_>,
) -> Result<SkillTemplateCompile> {
    let run_input = match request.run_input {
        RegressionExecutionInput::Resolved(input) => input.clone(),
        RegressionExecutionInput::Missing | RegressionExecutionInput::Ambiguous => Value::Null,
    };
    let goal = request.template.instantiate_goal(format!(
        "Validate the regression behavior of skill `{}`.",
        request.skill_name
    ));
    let source = ExecutionCompileSource::SkillRegression;
    let candidate = InitialCompileCandidate {
        kind: "initial",
        schema_version: 1,
        source,
        goal: &goal,
        plan: &request.template.plan,
        run_input: &run_input,
    };
    let candidate_bytes = canonical_json_bytes(&candidate)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let candidate_hash =
        execution_planning_hash("moa.execution.compile-candidate", &candidate_bytes);
    let approved_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(request.config.execution.max_cost_microusd),
        max_tokens: Some(request.config.execution.max_tokens),
        max_tasks: Some(request.config.execution.max_tasks),
        max_tool_calls: Some(request.config.execution.max_tool_calls),
        max_retrieved_bytes: Some(request.config.execution.max_retrieved_bytes),
        deadline_at: None,
    };
    let created_at = Utc::now();
    let started = Instant::now();
    let mut outcome = if matches!(request.run_input, RegressionExecutionInput::Ambiguous) {
        CompileExecutionOutcome {
            compiled: None,
            report: ExecutionValidationReport {
                issues: vec![moa_execution::ExecutionValidationIssue {
                    severity: ExecutionValidationSeverity::Error,
                    code: "ambiguous_run_input".to_string(),
                    path: "run_input".to_string(),
                    message: "skill regression suite declares multiple distinct structured inputs"
                        .to_string(),
                }],
            },
        }
    } else {
        compile(CompileExecutionRequest {
            goal,
            plan: request.template.plan.clone(),
            run_input: run_input.clone(),
            catalog: request.catalog.clone(),
            authorization: request.authorization.clone(),
            approved_budget,
            config: request.config.execution.clone(),
            now: created_at,
        })
    };
    match request.run_input {
        RegressionExecutionInput::Missing => {
            outcome.compiled = None;
            outcome
                .report
                .issues
                .push(moa_execution::ExecutionValidationIssue {
                    severity: ExecutionValidationSeverity::Error,
                    code: "missing_run_input".to_string(),
                    path: "run_input".to_string(),
                    message: "skill regression template requires explicit structured input"
                        .to_string(),
                });
        }
        RegressionExecutionInput::Resolved(_) => {
            if let Err(error) =
                validate_instance(request.skill_input_schema, &run_input, "skill_input_schema")
            {
                outcome.compiled = None;
                outcome
                    .report
                    .issues
                    .push(moa_execution::ExecutionValidationIssue {
                        severity: ExecutionValidationSeverity::Error,
                        code: "invalid_skill_input".to_string(),
                        path: "run_input".to_string(),
                        message: error.to_string(),
                    });
            }
        }
        RegressionExecutionInput::Ambiguous => {}
    }
    let duration_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let compile_outcome = classify_compile_outcome(&outcome);
    let validation_report = compiler_report_json(&outcome)?;
    let final_plan_hash = outcome
        .compiled
        .as_ref()
        .map(|compiled| compiled.plan.plan_hash.to_string());
    let accepted = compile_outcome == ExecutionCompileOutcome::Accepted;
    Ok(SkillTemplateCompile {
        audit: ExecutionPlanningAuditEnvelope {
            schema_version: 1,
            tenant_id: request.tenant_id,
            contact_id: None,
            session_id: None,
            originating_sequence: None,
            payload: ExecutionPlanningAuditPayload::Compile {
                source,
                operation_key: request.operation_key.to_string(),
                run_uid: None,
                plan_revision: None,
                outcome: compile_outcome,
                candidate_hash,
                final_plan_hash,
                validation_report,
                duration_micros,
                created_at,
            },
        },
        accepted,
    })
}

fn classify_compile_outcome(outcome: &CompileExecutionOutcome) -> ExecutionCompileOutcome {
    if outcome.compiled.is_some() && !outcome.report.has_errors() {
        return ExecutionCompileOutcome::Accepted;
    }
    let error_codes = outcome
        .report
        .issues
        .iter()
        .filter(|issue| issue.severity == ExecutionValidationSeverity::Error)
        .map(|issue| issue.code.as_str())
        .collect::<Vec<_>>();
    if error_codes.iter().any(|code| {
        matches!(
            *code,
            "missing_run_input"
                | "ambiguous_run_input"
                | "invalid_run_input"
                | "invalid_skill_input"
                | "empty_objective"
                | "goal_structure"
        )
    }) {
        ExecutionCompileOutcome::NeedsInput
    } else if error_codes.iter().any(|code| {
        code.contains("authorization")
            || code.contains("capability")
            || code.contains("budget")
            || code.contains("deadline")
            || code.starts_with("unsupported_")
            || *code == "skill_not_authorized"
    }) {
        ExecutionCompileOutcome::Unsupported
    } else {
        ExecutionCompileOutcome::Rejected
    }
}

fn compiler_report_json(outcome: &CompileExecutionOutcome) -> Result<String> {
    let violations = outcome
        .report
        .issues
        .iter()
        .map(|issue| ExecutionAuditViolation {
            code: issue.code.clone(),
            path: issue.path.clone(),
            message: issue.message.clone(),
        })
        .collect();
    let report = bounded_audit_report(true, violations)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    let bytes = canonical_json_bytes(&report)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    String::from_utf8(bytes).map_err(|error| MoaError::SerializationError(error.to_string()))
}

/// Reads the exact draft artifact revision identifier bound to a learning candidate.
pub(super) fn draft_artifact_revision_uid(candidate: &LearningCandidate) -> Result<uuid::Uuid> {
    let raw = candidate
        .payload
        .get("draft_artifact_revision_uid")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MoaError::ValidationError(
                "candidate payload missing draft_artifact_revision_uid".to_string(),
            )
        })?;
    uuid::Uuid::parse_str(raw).map_err(MoaError::from)
}

/// Hashes the validated suite for the compile-audit operation key.
pub(super) fn validated_suite_hash(suite: &TestSuite) -> Result<String> {
    let bytes = canonical_json_bytes(&json!({
        "schema_version": 1,
        "suite": suite,
    }))
    .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}
