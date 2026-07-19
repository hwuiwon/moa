//! Bounded model-assisted routing plus strict execution-plan generation.

use std::{str::FromStr, time::Instant};

use moa_artifacts::{
    canonical::canonical_json_bytes as artifact_canonical_json_bytes,
    execution_plan::{
        ExecutionGoalContract, GeneratedAmendmentCandidate, GeneratedExecutionCandidate,
    },
    reference::ArtifactRef,
};
use moa_core::{
    error::{MoaError, Result},
    traits::LLMProvider,
    types::execution_planning::{
        ExecutionAuditReport, ExecutionAuditViolation, ExecutionCompileOutcome,
        ExecutionCompileSource, ExecutionPlannerCallKind, ExecutionPlannerOutcome,
        ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload, ExecutionRouteDecision,
        ExecutionSourceProvenance, ExecutionStrategy, GeneratedPlanPlannerProvenance,
        bounded_audit_report, canonical_json_bytes, execution_planning_hash,
    },
};
use moa_execution::{
    CompileExecutionOutcome, CompileExecutionRequest, ExecutionValidationReport,
    ExecutionValidationSeverity, ValidateAmendmentRequest, compile, schema::validate_instance,
    validate_amendment,
};
use serde::Serialize;

pub mod request;
pub mod response;
pub mod routing;

pub use request::{
    AmendmentPlanningEvidence, EXECUTION_PLANNER_MAX_OUTPUT_TOKENS,
    EXECUTION_PLANNER_PROMPT_VERSION, ExecutionAmendmentPlanningRequest, ExecutionPlanningRequest,
};
pub use response::{
    AdmittedExecutionPlan, ExecutionAmendmentPlanningResult, ExecutionAmendmentPlanningResultKind,
    ExecutionPlanningResult, ExecutionPlanningResultKind,
};
pub use routing::{
    EXECUTION_ROUTER_DURABLE_CONFIDENCE_BPS, EXECUTION_ROUTER_HIGH_RISK_CONFIDENCE_BPS,
    EXECUTION_ROUTER_MAX_OUTPUT_TOKENS, EXECUTION_ROUTER_PROMPT_VERSION,
    EXECUTION_ROUTER_RESPONSE_MAX_BYTES, ExecutionRouteClassifierLabel,
    ExecutionRouteClassifierOutput, ExecutionRoutingInput, record_applied_route_audit,
    route_execution,
};

/// Raw planner candidate bytes are capped before parse or compilation.
pub const EXECUTION_PLANNER_CANDIDATE_MAX_BYTES: usize =
    moa_core::types::execution_planning::EXECUTION_CANDIDATE_MAX_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClassifiedCompileOutcome {
    Accepted,
    NeedsInput,
    Unsupported,
    Rejected,
}

/// Plans or instantiates one initial execution candidate over frozen session authority.
pub async fn plan_execution(
    provider: &dyn LLMProvider,
    request: ExecutionPlanningRequest,
) -> Result<ExecutionPlanningResult> {
    request
        .context
        .validate()
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    if request.objective.is_empty() {
        return Ok(ExecutionPlanningResult {
            kind: ExecutionPlanningResultKind::NeedsInput {
                message: "execution objective must not be empty".to_string(),
            },
            audits: Vec::new(),
        });
    }
    if let Some(invocation) = request.execution_template.clone() {
        return instantiate_template(&request, invocation).await;
    }
    plan_generated(provider, request).await
}

async fn instantiate_template(
    request: &ExecutionPlanningRequest,
    invocation: moa_core::types::execution_planning::ExecutionTemplateInvocation,
) -> Result<ExecutionPlanningResult> {
    let skill_ref = ArtifactRef::from_str(&invocation.template.skill_ref)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    if skill_ref
        .canonical_string()
        .map_err(|error| MoaError::ValidationError(error.to_string()))?
        != invocation.template.skill_ref
    {
        return Ok(unsupported(
            "selected skill reference is not canonical",
            Vec::new(),
        ));
    }
    let mut matches = request
        .context
        .execution_templates
        .iter()
        .filter(|template| {
            template.skill_ref == skill_ref
                && template.revision_uid == invocation.template.revision_uid
        });
    let Some(template) = matches.next() else {
        return Ok(unsupported(
            "selected execution template is not pinned and authorized",
            Vec::new(),
        ));
    };
    if matches.next().is_some() {
        return Ok(unsupported(
            "selected execution template revision is duplicated",
            Vec::new(),
        ));
    }
    if let Err(error) = validate_instance(
        &template.skill_input_schema,
        &invocation.input,
        "skill_input_schema",
    ) {
        return Ok(ExecutionPlanningResult {
            kind: ExecutionPlanningResultKind::NeedsInput {
                message: error.to_string(),
            },
            audits: Vec::new(),
        });
    }

    let candidate = GeneratedExecutionCandidate {
        goal: template
            .execution_plan
            .instantiate_goal(request.objective.clone()),
        plan: template.execution_plan.plan.clone(),
        run_input: invocation.input,
    };
    let operation_key = format!(
        "session:{}:{}:skill:{}",
        request.context.session_id,
        request.context.originating_user_sequence_num,
        template.revision_uid
    );
    let compiled = compile_candidate(
        request,
        &candidate,
        ExecutionCompileSource::SkillTemplate,
        &operation_key,
    )?;
    let audit = compile_audit(
        request,
        &compiled,
        ExecutionCompileSource::SkillTemplate,
        operation_key,
    );
    let audits = vec![audit];
    match compiled.classification {
        ClassifiedCompileOutcome::Accepted => {
            let compiled_plan = compiled.outcome.compiled.ok_or_else(|| {
                MoaError::ValidationError("accepted compile omitted plan".to_string())
            })?;
            let canonical_ref = template
                .skill_ref
                .canonical_string()
                .map_err(|error| MoaError::ValidationError(error.to_string()))?;
            Ok(ExecutionPlanningResult {
                kind: ExecutionPlanningResultKind::Ready(Box::new(AdmittedExecutionPlan {
                    compiled: compiled_plan,
                    run_input: candidate.run_input,
                    source_provenance: ExecutionSourceProvenance::SkillTemplate {
                        skill_template_ref: canonical_ref,
                        skill_template_revision_uid: template.revision_uid,
                    },
                    approved_budget: request.context.budget.clone(),
                })),
                audits,
            })
        }
        ClassifiedCompileOutcome::NeedsInput => Ok(ExecutionPlanningResult {
            kind: ExecutionPlanningResultKind::NeedsInput {
                message: "execution template input does not satisfy the pinned plan".to_string(),
            },
            audits,
        }),
        ClassifiedCompileOutcome::Unsupported | ClassifiedCompileOutcome::Rejected => Ok(
            unsupported("pinned execution template was rejected", audits),
        ),
    }
}

async fn plan_generated(
    provider: &dyn LLMProvider,
    request: ExecutionPlanningRequest,
) -> Result<ExecutionPlanningResult> {
    let initial_request = request::initial_completion_request(&request)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let initial_call = call_initial_provider(
        provider,
        &request,
        initial_request,
        ExecutionPlannerCallKind::InitialPlan,
        0,
    )
    .await?;
    let mut audits = vec![initial_call.audit.clone()];
    let ParsedProviderCall::Candidate {
        candidate,
        candidate_json,
        candidate_hash,
        model,
    } = initial_call.parsed
    else {
        return Ok(terminal_provider_result(initial_call.parsed, audits));
    };

    let first = compile_candidate(
        &request,
        &candidate,
        ExecutionCompileSource::GeneratedPlan,
        &generated_operation_key(&request, 0),
    )?;
    replace_planner_audit_after_compile(&mut audits[0], &first, &candidate_json)?;
    audits.push(compile_audit(
        &request,
        &first,
        ExecutionCompileSource::GeneratedPlan,
        generated_operation_key(&request, 0),
    ));
    if first.classification == ClassifiedCompileOutcome::Accepted {
        return admitted_generated(
            &request,
            *candidate,
            candidate_hash,
            model,
            first,
            0,
            audits,
        );
    }
    if first.classification != ClassifiedCompileOutcome::Rejected
        || request.config.planner_repair_attempts == 0
    {
        return Ok(classified_terminal(first.classification, audits));
    }

    let immutable_goal_json = canonical_string(&candidate.goal)?;
    let repair_request = request::initial_repair_completion_request(
        &request,
        &candidate_json,
        &immutable_goal_json,
        &first.report_json,
    )
    .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let repair_call = call_initial_provider(
        provider,
        &request,
        repair_request,
        ExecutionPlannerCallKind::InitialRepair,
        1,
    )
    .await?;
    audits.push(repair_call.audit.clone());
    let ParsedProviderCall::Candidate {
        candidate: repaired,
        candidate_json: repaired_json,
        candidate_hash: repaired_hash,
        model: repaired_model,
    } = repair_call.parsed
    else {
        return Ok(terminal_provider_result(repair_call.parsed, audits));
    };
    if canonical_string(&repaired.goal)? != immutable_goal_json {
        let report = bounded_audit_report(
            false,
            vec![ExecutionAuditViolation {
                code: "immutable_goal_changed".to_string(),
                path: "/goal".to_string(),
                message: "repair must preserve the complete immutable goal contract".to_string(),
            }],
        )
        .map_err(contract_error)?;
        let report_json = canonical_string(&report)?;
        let repair_audit = audits.last_mut().ok_or_else(|| {
            MoaError::ValidationError("repair audit was not recorded".to_string())
        })?;
        set_planner_audit(
            repair_audit,
            ExecutionPlannerOutcome::ImmutableGoalChanged,
            Some(repaired_hash),
            Some(repaired_json),
            Some(report_json),
        );
        return Ok(unsupported(
            "planner repair changed the immutable goal",
            audits,
        ));
    }

    let second = compile_candidate(
        &request,
        &repaired,
        ExecutionCompileSource::GeneratedPlan,
        &generated_operation_key(&request, 1),
    )?;
    let repair_audit = audits
        .last_mut()
        .ok_or_else(|| MoaError::ValidationError("repair audit was not recorded".to_string()))?;
    replace_planner_audit_after_compile(repair_audit, &second, &repaired_json)?;
    audits.push(compile_audit(
        &request,
        &second,
        ExecutionCompileSource::GeneratedPlan,
        generated_operation_key(&request, 1),
    ));
    if second.classification == ClassifiedCompileOutcome::Accepted {
        return admitted_generated(
            &request,
            *repaired,
            repaired_hash,
            repaired_model,
            second,
            1,
            audits,
        );
    }
    Ok(classified_terminal(second.classification, audits))
}

struct ProviderCall {
    parsed: ParsedProviderCall,
    audit: ExecutionPlanningAuditEnvelope,
}

enum ParsedProviderCall {
    Candidate {
        candidate: Box<GeneratedExecutionCandidate>,
        candidate_json: String,
        candidate_hash: String,
        model: String,
    },
    /// Planner-authored terminal verdict whose message is safe to surface.
    Unsupported(String),
    /// Provider/transport failure whose raw message must not reach a user.
    ProviderFailure(String),
}

async fn call_initial_provider(
    provider: &dyn LLMProvider,
    request: &ExecutionPlanningRequest,
    completion_request: moa_core::types::completion::CompletionRequest,
    call_kind: ExecutionPlannerCallKind,
    ordinal: u8,
) -> Result<ProviderCall> {
    let started = Instant::now();
    let response = match provider.complete(completion_request).await {
        Ok(stream) => match stream.collect().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(provider_error_call(
                    request,
                    call_kind,
                    ordinal,
                    request.planner_model.to_string(),
                    duration_micros(started),
                    error.to_string(),
                ));
            }
        },
        Err(error) => {
            return Ok(provider_error_call(
                request,
                call_kind,
                ordinal,
                request.planner_model.to_string(),
                duration_micros(started),
                error.to_string(),
            ));
        }
    };
    let raw = response.text;
    let duration = duration_micros(started);
    if raw.len() > EXECUTION_PLANNER_CANDIDATE_MAX_BYTES {
        let raw_hash = execution_planning_hash("moa.execution.planner-response", raw.as_bytes());
        let content_hash =
            execution_planning_hash("moa.execution.oversized-content", raw.as_bytes());
        let report = ExecutionAuditReport::Oversized {
            field: moa_core::types::execution_planning::ExecutionOversizedAuditField::Candidate,
            limit_bytes: EXECUTION_PLANNER_CANDIDATE_MAX_BYTES as u64,
            observed_bytes: u64::try_from(raw.len()).map_err(|_| {
                MoaError::ValidationError("planner response length does not fit u64".to_string())
            })?,
            content_hash,
        };
        let report_json = canonical_string(&report)?;
        return Ok(ProviderCall {
            parsed: ParsedProviderCall::Unsupported(
                "planner response exceeded its byte cap".to_string(),
            ),
            audit: planner_audit(
                request,
                call_kind,
                ordinal,
                ExecutionPlannerOutcome::Oversized,
                response.model.to_string(),
                Some(raw_hash),
                None,
                Some(report_json),
                duration,
            ),
        });
    }
    let candidate = match serde_json::from_str::<GeneratedExecutionCandidate>(&raw) {
        Ok(candidate) => candidate,
        Err(_) => {
            let raw_hash =
                execution_planning_hash("moa.execution.planner-response", raw.as_bytes());
            let report = bounded_audit_report(
                false,
                vec![ExecutionAuditViolation {
                    code: "invalid_generated_execution_candidate".to_string(),
                    path: "/".to_string(),
                    message: "provider response does not match GeneratedExecutionCandidate"
                        .to_string(),
                }],
            )
            .map_err(contract_error)?;
            return Ok(ProviderCall {
                parsed: ParsedProviderCall::Unsupported(
                    "planner response failed the strict response schema".to_string(),
                ),
                audit: planner_audit(
                    request,
                    call_kind,
                    ordinal,
                    ExecutionPlannerOutcome::SchemaRejected,
                    response.model.to_string(),
                    Some(raw_hash),
                    None,
                    Some(canonical_string(&report)?),
                    duration,
                ),
            });
        }
    };
    let candidate_json = canonical_string(&candidate)?;
    if candidate_json.len() > EXECUTION_PLANNER_CANDIDATE_MAX_BYTES {
        return Err(MoaError::ValidationError(
            "canonical planner candidate exceeds the byte cap".to_string(),
        ));
    }
    let candidate_hash =
        execution_planning_hash("moa.execution.planner-candidate", candidate_json.as_bytes());
    Ok(ProviderCall {
        parsed: ParsedProviderCall::Candidate {
            candidate: Box::new(candidate),
            candidate_json: candidate_json.clone(),
            candidate_hash: candidate_hash.clone(),
            model: response.model.to_string(),
        },
        audit: planner_audit(
            request,
            call_kind,
            ordinal,
            ExecutionPlannerOutcome::Accepted,
            response.model.to_string(),
            Some(candidate_hash),
            Some(candidate_json),
            None,
            duration,
        ),
    })
}

fn provider_error_call(
    request: &ExecutionPlanningRequest,
    call_kind: ExecutionPlannerCallKind,
    ordinal: u8,
    model: String,
    duration: u64,
    message: String,
) -> ProviderCall {
    ProviderCall {
        parsed: ParsedProviderCall::ProviderFailure(message),
        audit: planner_audit(
            request,
            call_kind,
            ordinal,
            ExecutionPlannerOutcome::ProviderError,
            model,
            None,
            None,
            None,
            duration,
        ),
    }
}

struct CandidateCompile {
    outcome: CompileExecutionOutcome,
    classification: ClassifiedCompileOutcome,
    candidate_hash: String,
    report_json: String,
    report_hash: String,
    duration_micros: u64,
}

fn compile_candidate(
    request: &ExecutionPlanningRequest,
    candidate: &GeneratedExecutionCandidate,
    source: ExecutionCompileSource,
    _operation_key: &str,
) -> Result<CandidateCompile> {
    let preimage = InitialCompileCandidate {
        kind: "initial",
        schema_version: 1,
        source,
        goal: &candidate.goal,
        plan: &candidate.plan,
        run_input: &candidate.run_input,
    };
    let candidate_hash = execution_planning_hash(
        "moa.execution.compile-candidate",
        &artifact_canonical_json_bytes(&preimage)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
    );
    let started = Instant::now();
    let mut outcome = compile(CompileExecutionRequest {
        goal: candidate.goal.clone(),
        plan: candidate.plan.clone(),
        run_input: candidate.run_input.clone(),
        catalog: request.context.catalog.clone(),
        authorization: request.context.authorization.clone(),
        approved_budget: request.context.budget.clone(),
        config: request.config.clone(),
        now: request.now,
    });
    if candidate.goal.objective != request.objective {
        outcome.compiled = None;
        outcome
            .report
            .issues
            .push(moa_execution::ExecutionValidationIssue {
                severity: ExecutionValidationSeverity::Error,
                code: "objective_mismatch".to_string(),
                path: "goal.objective".to_string(),
                message: "goal objective must equal the persisted user message".to_string(),
            });
    }
    let duration_micros = duration_micros(started);
    let classification = classify_report(&outcome);
    let report = compiler_audit_report(&outcome.report)?;
    let report_hash = report_hash(&report).to_string();
    let report_json = canonical_string(&report)?;
    Ok(CandidateCompile {
        outcome,
        classification,
        candidate_hash,
        report_json,
        report_hash,
        duration_micros,
    })
}

#[derive(Serialize)]
struct InitialCompileCandidate<'a> {
    kind: &'static str,
    schema_version: u8,
    source: ExecutionCompileSource,
    goal: &'a ExecutionGoalContract,
    plan: &'a moa_artifacts::execution_plan::ExecutionPlanDefinition,
    run_input: &'a serde_json::Value,
}

fn classify_report(outcome: &CompileExecutionOutcome) -> ClassifiedCompileOutcome {
    classify_validation_report(&outcome.report, outcome.compiled.is_some())
}

fn classify_validation_report(
    report: &ExecutionValidationReport,
    has_compiled_value: bool,
) -> ClassifiedCompileOutcome {
    if has_compiled_value && !report.has_errors() {
        return ClassifiedCompileOutcome::Accepted;
    }
    let error_codes = report
        .issues
        .iter()
        .filter(|issue| issue.severity == ExecutionValidationSeverity::Error)
        .map(|issue| issue.code.as_str())
        .collect::<Vec<_>>();
    if error_codes.iter().any(|code| {
        matches!(
            *code,
            "invalid_run_input" | "empty_objective" | "goal_structure"
        )
    }) {
        ClassifiedCompileOutcome::NeedsInput
    } else if error_codes.iter().any(|code| {
        code.contains("authorization")
            || code.contains("capability")
            || code.contains("budget")
            || code.contains("deadline")
            || code.starts_with("unsupported_")
            || *code == "skill_not_authorized"
            || *code == "objective_mismatch"
    }) {
        ClassifiedCompileOutcome::Unsupported
    } else {
        ClassifiedCompileOutcome::Rejected
    }
}

fn compiler_audit_report(report: &ExecutionValidationReport) -> Result<ExecutionAuditReport> {
    let violations = report
        .issues
        .iter()
        .map(|issue| ExecutionAuditViolation {
            code: issue.code.clone(),
            path: issue.path.clone(),
            message: issue.message.clone(),
        })
        .collect();
    bounded_audit_report(true, violations).map_err(contract_error)
}

fn report_hash(report: &ExecutionAuditReport) -> &str {
    match report {
        ExecutionAuditReport::Schema {
            full_report_hash, ..
        }
        | ExecutionAuditReport::Compiler {
            full_report_hash, ..
        } => full_report_hash,
        ExecutionAuditReport::Oversized { content_hash, .. } => content_hash,
    }
}

fn replace_planner_audit_after_compile(
    audit: &mut ExecutionPlanningAuditEnvelope,
    compiled: &CandidateCompile,
    candidate_json: &str,
) -> Result<()> {
    let outcome = match compiled.classification {
        ClassifiedCompileOutcome::Accepted => ExecutionPlannerOutcome::Accepted,
        ClassifiedCompileOutcome::NeedsInput => ExecutionPlannerOutcome::NeedsInput,
        ClassifiedCompileOutcome::Unsupported => ExecutionPlannerOutcome::Unsupported,
        ClassifiedCompileOutcome::Rejected => ExecutionPlannerOutcome::CompilerRejected,
    };
    let candidate_hash =
        execution_planning_hash("moa.execution.planner-candidate", candidate_json.as_bytes());
    set_planner_audit(
        audit,
        outcome,
        Some(candidate_hash),
        Some(candidate_json.to_string()),
        Some(compiled.report_json.clone()),
    );
    Ok(())
}

fn set_planner_audit(
    audit: &mut ExecutionPlanningAuditEnvelope,
    outcome: ExecutionPlannerOutcome,
    candidate_hash: Option<String>,
    candidate_json: Option<String>,
    compiler_report: Option<String>,
) {
    let ExecutionPlanningAuditPayload::PlannerCall {
        outcome: stored_outcome,
        candidate_hash: stored_hash,
        candidate_json: stored_candidate,
        compiler_report: stored_report,
        ..
    } = &mut audit.payload
    else {
        return;
    };
    *stored_outcome = outcome;
    *stored_hash = candidate_hash;
    *stored_candidate = candidate_json;
    *stored_report = compiler_report;
}

#[allow(
    clippy::too_many_arguments,
    reason = "the strict audit envelope keeps every persisted cohort field explicit"
)]
fn planner_audit(
    request: &ExecutionPlanningRequest,
    call_kind: ExecutionPlannerCallKind,
    call_ordinal: u8,
    outcome: ExecutionPlannerOutcome,
    provider_model: String,
    candidate_hash: Option<String>,
    candidate_json: Option<String>,
    compiler_report: Option<String>,
    duration_micros: u64,
) -> ExecutionPlanningAuditEnvelope {
    ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id: request.context.tenant_id,
        contact_id: request.context.contact_id,
        session_id: Some(request.context.session_id),
        originating_sequence: Some(request.context.originating_user_sequence_num),
        payload: ExecutionPlanningAuditPayload::PlannerCall {
            call_kind,
            call_ordinal,
            run_uid: None,
            plan_revision: None,
            outcome,
            provider_model,
            prompt_version: EXECUTION_PLANNER_PROMPT_VERSION.to_string(),
            candidate_hash,
            candidate_json,
            compiler_report,
            duration_micros,
            created_at: request.now,
        },
    }
}

fn compile_audit(
    request: &ExecutionPlanningRequest,
    compiled: &CandidateCompile,
    source: ExecutionCompileSource,
    operation_key: String,
) -> ExecutionPlanningAuditEnvelope {
    let outcome = match compiled.classification {
        ClassifiedCompileOutcome::Accepted => ExecutionCompileOutcome::Accepted,
        ClassifiedCompileOutcome::NeedsInput => ExecutionCompileOutcome::NeedsInput,
        ClassifiedCompileOutcome::Unsupported => ExecutionCompileOutcome::Unsupported,
        ClassifiedCompileOutcome::Rejected => ExecutionCompileOutcome::Rejected,
    };
    ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id: request.context.tenant_id,
        contact_id: request.context.contact_id,
        session_id: Some(request.context.session_id),
        originating_sequence: Some(request.context.originating_user_sequence_num),
        payload: ExecutionPlanningAuditPayload::Compile {
            source,
            operation_key,
            run_uid: None,
            plan_revision: None,
            outcome,
            candidate_hash: compiled.candidate_hash.clone(),
            final_plan_hash: compiled
                .outcome
                .compiled
                .as_ref()
                .map(|plan| plan.plan.plan_hash.to_string()),
            validation_report: compiled.report_json.clone(),
            duration_micros: compiled.duration_micros,
            created_at: request.now,
        },
    }
}

fn admitted_generated(
    request: &ExecutionPlanningRequest,
    candidate: GeneratedExecutionCandidate,
    candidate_hash: String,
    model: String,
    compiled: CandidateCompile,
    repair_attempts: u8,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> Result<ExecutionPlanningResult> {
    let compiled_plan = compiled
        .outcome
        .compiled
        .ok_or_else(|| MoaError::ValidationError("accepted compile omitted plan".to_string()))?;
    let final_plan_hash = compiled_plan.plan.plan_hash.to_string();
    Ok(ExecutionPlanningResult {
        kind: ExecutionPlanningResultKind::Ready(Box::new(AdmittedExecutionPlan {
            compiled: compiled_plan,
            run_input: candidate.run_input,
            source_provenance: ExecutionSourceProvenance::GeneratedPlan {
                planner: GeneratedPlanPlannerProvenance {
                    model,
                    prompt_version: EXECUTION_PLANNER_PROMPT_VERSION.to_string(),
                    candidate_hash,
                    compiler_report_hash: compiled.report_hash,
                    final_plan_hash,
                    repair_attempts,
                },
            },
            approved_budget: request.context.budget.clone(),
        })),
        audits,
    })
}

fn terminal_provider_result(
    parsed: ParsedProviderCall,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionPlanningResult {
    match parsed {
        ParsedProviderCall::Unsupported(message) => unsupported(message, audits),
        ParsedProviderCall::ProviderFailure(message) => provider_failure(message, audits),
        ParsedProviderCall::Candidate { .. } => unsupported("invalid planner state", audits),
    }
}

fn classified_terminal(
    classification: ClassifiedCompileOutcome,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionPlanningResult {
    match classification {
        ClassifiedCompileOutcome::NeedsInput => ExecutionPlanningResult {
            kind: ExecutionPlanningResultKind::NeedsInput {
                message: "execution planning requires structured input".to_string(),
            },
            audits,
        },
        ClassifiedCompileOutcome::Accepted => unsupported("invalid accepted planner state", audits),
        ClassifiedCompileOutcome::Unsupported => unsupported(
            "execution plan is unsupported by the frozen authority",
            audits,
        ),
        ClassifiedCompileOutcome::Rejected => {
            unsupported("execution plan remained compiler-rejected", audits)
        }
    }
}

fn unsupported(
    message: impl Into<String>,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionPlanningResult {
    ExecutionPlanningResult {
        kind: ExecutionPlanningResultKind::Unsupported {
            message: message.into(),
        },
        audits,
    }
}

/// Wraps a raw provider/transport failure so callers can keep it out of user text.
fn provider_failure(
    message: impl Into<String>,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionPlanningResult {
    ExecutionPlanningResult {
        kind: ExecutionPlanningResultKind::ProviderFailure {
            message: message.into(),
        },
        audits,
    }
}

fn generated_operation_key(request: &ExecutionPlanningRequest, ordinal: u8) -> String {
    format!(
        "session:{}:{}:generated:{ordinal}",
        request.context.session_id, request.context.originating_user_sequence_num
    )
}

fn canonical_string<T: Serialize>(value: &T) -> Result<String> {
    let bytes = canonical_json_bytes(value).map_err(contract_error)?;
    String::from_utf8(bytes).map_err(|error| MoaError::SerializationError(error.to_string()))
}

fn duration_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn contract_error(
    error: moa_core::types::execution_planning::ExecutionPlanningContractError,
) -> MoaError {
    MoaError::ValidationError(error.to_string())
}

/// Returns whether a rejected pinned template may safely fall back to Inline Execute.
#[must_use]
pub fn pinned_template_may_fallback_to_inline(
    independent_route: &ExecutionRouteDecision,
    report: &ExecutionValidationReport,
) -> bool {
    if !matches!(
        independent_route,
        ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            ..
        }
    ) {
        return false;
    }
    !report.issues.iter().any(|issue| {
        if issue.severity != ExecutionValidationSeverity::Error {
            return false;
        }
        let code = issue.code.as_str();
        code.contains("input")
            || code.contains("capability")
            || code.contains("authorization")
            || code.contains("budget")
            || code.contains("fanout")
            || code.contains("fan_out")
            || code.contains("review")
            || code.contains("signal")
            || code.contains("durable")
            || code.contains("resum")
            || code == "skill_not_authorized"
    })
}

/// Generates and validates one restricted plan amendment with at most one repair call.
pub async fn plan_amendment(
    provider: &dyn LLMProvider,
    request: ExecutionAmendmentPlanningRequest,
) -> Result<ExecutionAmendmentPlanningResult> {
    request
        .context
        .validate()
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    if request.base_plan_revision != request.evidence.projection.plan_revision {
        return Ok(amendment_unsupported(
            "amendment base revision does not match the persisted projection",
            Vec::new(),
        ));
    }

    let completion = request::amendment_completion_request(&request, None)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let first_call = call_amendment_provider(
        provider,
        &request,
        completion,
        ExecutionPlannerCallKind::Amendment,
        0,
    )
    .await?;
    let mut audits = vec![first_call.audit.clone()];
    let ParsedAmendmentCall::Candidate {
        candidate,
        candidate_json,
        candidate_hash,
    } = first_call.parsed
    else {
        return Ok(amendment_terminal_provider(first_call.parsed, audits));
    };
    let first = compile_amendment_candidate(&request, &candidate)?;
    replace_amendment_planner_audit_after_compile(&mut audits[0], &first, &candidate_json)?;
    audits.push(amendment_compile_audit(&request, &first));
    if first.classification == ClassifiedCompileOutcome::Accepted {
        return admitted_amendment(candidate, candidate_hash, first, audits);
    }
    if first.classification != ClassifiedCompileOutcome::Rejected
        || request.config.planner_repair_attempts == 0
    {
        return Ok(amendment_classified_terminal(first.classification, audits));
    }

    let completion = request::amendment_completion_request(
        &request,
        Some((&candidate_json, &first.report_json)),
    )
    .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let repair_call = call_amendment_provider(
        provider,
        &request,
        completion,
        ExecutionPlannerCallKind::AmendmentRepair,
        1,
    )
    .await?;
    audits.push(repair_call.audit.clone());
    let ParsedAmendmentCall::Candidate {
        candidate: repaired,
        candidate_json: repaired_json,
        candidate_hash: repaired_hash,
    } = repair_call.parsed
    else {
        return Ok(amendment_terminal_provider(repair_call.parsed, audits));
    };
    let second = compile_amendment_candidate(&request, &repaired)?;
    let repair_audit = audits.last_mut().ok_or_else(|| {
        MoaError::ValidationError("amendment repair audit was not recorded".to_string())
    })?;
    replace_amendment_planner_audit_after_compile(repair_audit, &second, &repaired_json)?;
    audits.push(amendment_compile_audit(&request, &second));
    if second.classification == ClassifiedCompileOutcome::Accepted {
        admitted_amendment(repaired, repaired_hash, second, audits)
    } else {
        Ok(amendment_classified_terminal(second.classification, audits))
    }
}

struct AmendmentProviderCall {
    parsed: ParsedAmendmentCall,
    audit: ExecutionPlanningAuditEnvelope,
}

enum ParsedAmendmentCall {
    Candidate {
        candidate: GeneratedAmendmentCandidate,
        candidate_json: String,
        candidate_hash: String,
    },
    /// Planner-authored terminal verdict whose message is safe to surface.
    Unsupported(String),
    /// Provider/transport failure whose raw message must not reach a user.
    ProviderFailure(String),
}

async fn call_amendment_provider(
    provider: &dyn LLMProvider,
    request: &ExecutionAmendmentPlanningRequest,
    completion: moa_core::types::completion::CompletionRequest,
    call_kind: ExecutionPlannerCallKind,
    ordinal: u8,
) -> Result<AmendmentProviderCall> {
    let started = Instant::now();
    let response = match provider.complete(completion).await {
        Ok(stream) => match stream.collect().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(amendment_provider_error(
                    request,
                    call_kind,
                    ordinal,
                    request.planner_model.to_string(),
                    duration_micros(started),
                    error.to_string(),
                ));
            }
        },
        Err(error) => {
            return Ok(amendment_provider_error(
                request,
                call_kind,
                ordinal,
                request.planner_model.to_string(),
                duration_micros(started),
                error.to_string(),
            ));
        }
    };
    let raw = response.text;
    let duration = duration_micros(started);
    if raw.len() > EXECUTION_PLANNER_CANDIDATE_MAX_BYTES {
        let raw_hash = execution_planning_hash("moa.execution.planner-response", raw.as_bytes());
        let report = ExecutionAuditReport::Oversized {
            field: moa_core::types::execution_planning::ExecutionOversizedAuditField::Candidate,
            limit_bytes: EXECUTION_PLANNER_CANDIDATE_MAX_BYTES as u64,
            observed_bytes: u64::try_from(raw.len()).map_err(|_| {
                MoaError::ValidationError("planner response length does not fit u64".to_string())
            })?,
            content_hash: execution_planning_hash(
                "moa.execution.oversized-content",
                raw.as_bytes(),
            ),
        };
        return Ok(AmendmentProviderCall {
            parsed: ParsedAmendmentCall::Unsupported(
                "amendment response exceeded its byte cap".to_string(),
            ),
            audit: amendment_planner_audit(
                request,
                call_kind,
                ordinal,
                ExecutionPlannerOutcome::Oversized,
                response.model.to_string(),
                Some(raw_hash),
                None,
                Some(canonical_string(&report)?),
                duration,
            ),
        });
    }
    let candidate = match serde_json::from_str::<GeneratedAmendmentCandidate>(&raw) {
        Ok(candidate) => candidate,
        Err(_) => {
            let report = bounded_audit_report(
                false,
                vec![ExecutionAuditViolation {
                    code: "invalid_generated_amendment_candidate".to_string(),
                    path: "/".to_string(),
                    message: "provider response does not match GeneratedAmendmentCandidate"
                        .to_string(),
                }],
            )
            .map_err(contract_error)?;
            return Ok(AmendmentProviderCall {
                parsed: ParsedAmendmentCall::Unsupported(
                    "amendment response failed the strict response schema".to_string(),
                ),
                audit: amendment_planner_audit(
                    request,
                    call_kind,
                    ordinal,
                    ExecutionPlannerOutcome::SchemaRejected,
                    response.model.to_string(),
                    Some(execution_planning_hash(
                        "moa.execution.planner-response",
                        raw.as_bytes(),
                    )),
                    None,
                    Some(canonical_string(&report)?),
                    duration,
                ),
            });
        }
    };
    let candidate_json = canonical_string(&candidate)?;
    let candidate_hash =
        execution_planning_hash("moa.execution.planner-candidate", candidate_json.as_bytes());
    Ok(AmendmentProviderCall {
        parsed: ParsedAmendmentCall::Candidate {
            candidate,
            candidate_json: candidate_json.clone(),
            candidate_hash: candidate_hash.clone(),
        },
        audit: amendment_planner_audit(
            request,
            call_kind,
            ordinal,
            ExecutionPlannerOutcome::Accepted,
            response.model.to_string(),
            Some(candidate_hash),
            Some(candidate_json),
            None,
            duration,
        ),
    })
}

fn amendment_provider_error(
    request: &ExecutionAmendmentPlanningRequest,
    call_kind: ExecutionPlannerCallKind,
    ordinal: u8,
    model: String,
    duration: u64,
    message: String,
) -> AmendmentProviderCall {
    AmendmentProviderCall {
        parsed: ParsedAmendmentCall::ProviderFailure(message),
        audit: amendment_planner_audit(
            request,
            call_kind,
            ordinal,
            ExecutionPlannerOutcome::ProviderError,
            model,
            None,
            None,
            None,
            duration,
        ),
    }
}

struct AmendmentCompile {
    outcome: moa_execution::AmendmentValidationOutcome,
    classification: ClassifiedCompileOutcome,
    compile_candidate_hash: String,
    report_json: String,
    duration_micros: u64,
}

fn compile_amendment_candidate(
    request: &ExecutionAmendmentPlanningRequest,
    candidate: &GeneratedAmendmentCandidate,
) -> Result<AmendmentCompile> {
    #[derive(Serialize)]
    struct AmendmentCompileCandidate<'a> {
        kind: &'static str,
        schema_version: u8,
        source: &'static str,
        goal: &'a ExecutionGoalContract,
        base_plan_hash: String,
        amendment: &'a moa_artifacts::execution_plan::PlanAmendment,
    }
    let preimage = AmendmentCompileCandidate {
        kind: "amendment",
        schema_version: 1,
        source: "amendment",
        goal: &request.evidence.goal,
        base_plan_hash: request.evidence.active_plan.plan_hash.to_string(),
        amendment: &candidate.amendment,
    };
    let compile_candidate_hash = execution_planning_hash(
        "moa.execution.compile-candidate",
        &artifact_canonical_json_bytes(&preimage)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
    );
    let started = Instant::now();
    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: request.evidence.goal.clone(),
        active_plan: request.evidence.active_plan.clone(),
        amendment: candidate.amendment.clone(),
        projection: request.evidence.projection.clone(),
        catalog: request.context.catalog.clone(),
        authorization: request.context.authorization.clone(),
        remaining_budget: request.remaining_budget.clone(),
        config: request.config.clone(),
        now: request.now,
    });
    let duration_micros = duration_micros(started);
    let classification = classify_validation_report(&outcome.report, outcome.plan.is_some());
    let report_json = canonical_string(&compiler_audit_report(&outcome.report)?)?;
    Ok(AmendmentCompile {
        outcome,
        classification,
        compile_candidate_hash,
        report_json,
        duration_micros,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the strict amendment audit keeps every persisted cohort field explicit"
)]
fn amendment_planner_audit(
    request: &ExecutionAmendmentPlanningRequest,
    call_kind: ExecutionPlannerCallKind,
    call_ordinal: u8,
    outcome: ExecutionPlannerOutcome,
    provider_model: String,
    candidate_hash: Option<String>,
    candidate_json: Option<String>,
    compiler_report: Option<String>,
    duration_micros: u64,
) -> ExecutionPlanningAuditEnvelope {
    ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id: request.context.tenant_id,
        contact_id: request.context.contact_id,
        session_id: Some(request.context.session_id),
        originating_sequence: Some(request.context.originating_user_sequence_num),
        payload: ExecutionPlanningAuditPayload::PlannerCall {
            call_kind,
            call_ordinal,
            run_uid: Some(request.run_uid),
            plan_revision: Some(request.base_plan_revision),
            outcome,
            provider_model,
            prompt_version: EXECUTION_PLANNER_PROMPT_VERSION.to_string(),
            candidate_hash,
            candidate_json,
            compiler_report,
            duration_micros,
            created_at: request.now,
        },
    }
}

fn replace_amendment_planner_audit_after_compile(
    audit: &mut ExecutionPlanningAuditEnvelope,
    compiled: &AmendmentCompile,
    candidate_json: &str,
) -> Result<()> {
    let outcome = planner_outcome(compiled.classification);
    set_planner_audit(
        audit,
        outcome,
        Some(execution_planning_hash(
            "moa.execution.planner-candidate",
            candidate_json.as_bytes(),
        )),
        Some(candidate_json.to_string()),
        Some(compiled.report_json.clone()),
    );
    Ok(())
}

fn planner_outcome(classification: ClassifiedCompileOutcome) -> ExecutionPlannerOutcome {
    match classification {
        ClassifiedCompileOutcome::Accepted => ExecutionPlannerOutcome::Accepted,
        ClassifiedCompileOutcome::NeedsInput => ExecutionPlannerOutcome::NeedsInput,
        ClassifiedCompileOutcome::Unsupported => ExecutionPlannerOutcome::Unsupported,
        ClassifiedCompileOutcome::Rejected => ExecutionPlannerOutcome::CompilerRejected,
    }
}

fn amendment_compile_audit(
    request: &ExecutionAmendmentPlanningRequest,
    compiled: &AmendmentCompile,
) -> ExecutionPlanningAuditEnvelope {
    ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id: request.context.tenant_id,
        contact_id: request.context.contact_id,
        session_id: Some(request.context.session_id),
        originating_sequence: Some(request.context.originating_user_sequence_num),
        payload: ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::Amendment,
            operation_key: format!(
                "run:{}:{}:amendment:{}",
                request.run_uid, request.base_plan_revision, compiled.compile_candidate_hash
            ),
            run_uid: Some(request.run_uid),
            plan_revision: Some(request.base_plan_revision),
            outcome: match compiled.classification {
                ClassifiedCompileOutcome::Accepted => ExecutionCompileOutcome::Accepted,
                ClassifiedCompileOutcome::NeedsInput => ExecutionCompileOutcome::NeedsInput,
                ClassifiedCompileOutcome::Unsupported => ExecutionCompileOutcome::Unsupported,
                ClassifiedCompileOutcome::Rejected => ExecutionCompileOutcome::Rejected,
            },
            candidate_hash: compiled.compile_candidate_hash.clone(),
            final_plan_hash: compiled
                .outcome
                .plan
                .as_ref()
                .map(|plan| plan.plan_hash.to_string()),
            validation_report: compiled.report_json.clone(),
            duration_micros: compiled.duration_micros,
            created_at: request.now,
        },
    }
}

fn admitted_amendment(
    candidate: GeneratedAmendmentCandidate,
    candidate_hash: String,
    compiled: AmendmentCompile,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> Result<ExecutionAmendmentPlanningResult> {
    let plan = compiled.outcome.plan.ok_or_else(|| {
        MoaError::ValidationError("accepted amendment compile omitted plan".to_string())
    })?;
    Ok(ExecutionAmendmentPlanningResult {
        kind: ExecutionAmendmentPlanningResultKind::Ready {
            plan: Box::new(plan),
            amendment: candidate.amendment,
            candidate_hash,
        },
        audits,
    })
}

fn amendment_terminal_provider(
    parsed: ParsedAmendmentCall,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionAmendmentPlanningResult {
    match parsed {
        ParsedAmendmentCall::Unsupported(message) => amendment_unsupported(message, audits),
        ParsedAmendmentCall::ProviderFailure(message) => {
            amendment_provider_failure(message, audits)
        }
        ParsedAmendmentCall::Candidate { .. } => {
            amendment_unsupported("invalid amendment planner state", audits)
        }
    }
}

fn amendment_classified_terminal(
    classification: ClassifiedCompileOutcome,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionAmendmentPlanningResult {
    match classification {
        ClassifiedCompileOutcome::NeedsInput => ExecutionAmendmentPlanningResult {
            kind: ExecutionAmendmentPlanningResultKind::NeedsInput {
                message: "amendment requires caller input".to_string(),
            },
            audits,
        },
        ClassifiedCompileOutcome::Accepted => {
            amendment_unsupported("invalid accepted amendment state", audits)
        }
        ClassifiedCompileOutcome::Unsupported => {
            amendment_unsupported("amendment exceeds frozen authority or budget", audits)
        }
        ClassifiedCompileOutcome::Rejected => {
            amendment_unsupported("amendment remained compiler-rejected", audits)
        }
    }
}

fn amendment_unsupported(
    message: impl Into<String>,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionAmendmentPlanningResult {
    ExecutionAmendmentPlanningResult {
        kind: ExecutionAmendmentPlanningResultKind::Unsupported {
            message: message.into(),
        },
        audits,
    }
}

/// Wraps a raw amendment provider/transport failure so callers can keep it out of user text.
fn amendment_provider_failure(
    message: impl Into<String>,
    audits: Vec<ExecutionPlanningAuditEnvelope>,
) -> ExecutionAmendmentPlanningResult {
    ExecutionAmendmentPlanningResult {
        kind: ExecutionAmendmentPlanningResultKind::ProviderFailure {
            message: message.into(),
        },
        audits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_planning_pinned_template_fallback_is_gap_closed() {
        // Pins: Inline fallback is disallowed for every authority/input/execution-shape gap.
        let inline = ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            rationale: "This request can finish in a bounded interactive loop.".to_string(),
        };
        let structural = ExecutionValidationReport {
            issues: vec![moa_execution::ExecutionValidationIssue {
                severity: ExecutionValidationSeverity::Error,
                code: "plan_structure".to_string(),
                path: "plan".to_string(),
                message: "invalid".to_string(),
            }],
        };
        assert!(pinned_template_may_fallback_to_inline(&inline, &structural));
        for code in [
            "invalid_run_input",
            "capability_not_in_catalog",
            "capability_not_authorized",
            "approved_budget_exceeded",
            "review_gap",
            "signal_gap",
            "durable_gap",
            "resumability_gap",
        ] {
            let report = ExecutionValidationReport {
                issues: vec![moa_execution::ExecutionValidationIssue {
                    severity: ExecutionValidationSeverity::Error,
                    code: code.to_string(),
                    path: "plan".to_string(),
                    message: "invalid".to_string(),
                }],
            };
            assert!(
                !pinned_template_may_fallback_to_inline(&inline, &report),
                "{code}"
            );
        }
    }
}
