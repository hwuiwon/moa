//! Review-boundary regression reporting for proposed skill updates.

use std::sync::Arc;
use std::{future::Future, pin::Pin, time::Instant};

use chrono::Utc;
use moa_artifacts::{
    canonical::canonical_json_bytes as artifact_canonical_json_bytes,
    execution_plan::{
        ExecutionBudgetLimit, ExecutionGoalContract, ExecutionPlanDefinition, ExecutionPlanTemplate,
    },
    registry::{
        ArtifactFile, ArtifactRegistry, StoredArtifactRevision, StoredSuiteContribution,
        SuiteContributionKind,
    },
};
use moa_config::MoaConfig;
use moa_core::{error::MoaError, traits::LLMProvider, types::provider::ModelTask};
use moa_core::{
    error::Result,
    types::{
        action_policy::ActionRuleScope,
        execution_planning::{
            ExecutionAuditViolation, ExecutionCompileOutcome, ExecutionCompileSource,
            ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload, bounded_audit_report,
            execution_planning_hash,
        },
        experience::LearningCandidate,
        identifiers::TenantId,
    },
};
use moa_eval::EvalEngine;
use moa_eval_core::engine::EvalRun;
use moa_eval_core::{
    ActionPolicyOverride, AgentConfig, EngineOptions, EvalResult, EvalScoreValue, EvalStatus,
    Evaluator, EvaluatorOptions, InstructionOverride, TestSuite, build_evaluators, evaluate_run,
};
use moa_execution::{
    CompileExecutionOutcome, CompileExecutionRequest, ExecutionAuthorizationEnvelope,
    ExecutionCapabilityCatalog, ExecutionValidationReport, ExecutionValidationSeverity, compile,
    repository::{CompileAuditWriteOutcome, ExecutionRepository, ExecutionScope},
    schema::validate_instance,
};
use moa_hands::ToolRouter;
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use moa_skills::artifact::skill_definition_from_package;
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::registry::SkillRegistry;
use moa_skills::regression::{SkillRegressionSummary, compare_scores};
use serde::Serialize;
use serde_json::{Value, json};

use crate::services::execution::resolve_skill_regression_compile_authority;

const DEFAULT_SKILL_TEST_BUDGET_DOLLARS: f64 = 0.50;
const DEFAULT_SKILL_EVALUATORS: &[&str] = &["trajectory", "output", "tool_success"];
/// Floor applied when a generated suite carries no (or a zero) case timeout.
const DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS: u64 = 90;
const EXECUTION_INPUT_METADATA_KEY: &str = "execution_input";

type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// What the review-time regression gate actually executed.
///
/// Acceptance checks recorded on a promoted candidate are derived from this
/// value, so it must describe reality: the gate either compared the candidate
/// against the previous active skill, smoke-ran the candidate alone because no
/// previous revision exists, or blocked promotion without a passing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillRegressionExecution {
    /// Previous and candidate suites both executed and scores were compared.
    ComparedWithPrevious,
    /// No previous active skill existed; the candidate suite executed alone.
    CandidateOnly,
    /// Nothing passed; promotion is blocked with a rejection reason.
    Blocked,
}

impl SkillRegressionExecution {
    /// Returns the stable snake_case label recorded in gate reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ComparedWithPrevious => "compared_with_previous",
            Self::CandidateOnly => "candidate_only",
            Self::Blocked => "blocked",
        }
    }
}

/// Outcome of review-time regression evaluation for a skill proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRegressionGate {
    /// Structured report to attach to the candidate evaluation payload.
    pub report: Value,
    /// Whether review acceptance may continue to publish and materialize the skill.
    pub allow_promotion: bool,
    /// Human-readable rejection reason when regression blocks promotion.
    pub rejection_reason: Option<String>,
    /// What the gate actually executed, for honest acceptance-check derivation.
    pub execution: SkillRegressionExecution,
    /// Number of held-out suite sources (prior revisions + sibling sessions)
    /// that actually executed, for honest acceptance-check derivation.
    pub held_out_sources: usize,
    /// Exact compile-audit operation key required by the terminal candidate CAS.
    pub compile_operation_key: Option<String>,
}

impl SkillRegressionGate {
    fn accepted(
        report: Value,
        execution: SkillRegressionExecution,
        held_out_sources: usize,
    ) -> Self {
        Self {
            report,
            allow_promotion: true,
            rejection_reason: None,
            execution,
            held_out_sources,
            compile_operation_key: None,
        }
    }

    fn blocked(report: Value, rejection_reason: String) -> Self {
        Self {
            report,
            allow_promotion: false,
            rejection_reason: Some(rejection_reason),
            execution: SkillRegressionExecution::Blocked,
            held_out_sources: 0,
            compile_operation_key: None,
        }
    }

    fn with_compile_operation_key(mut self, operation_key: Option<String>) -> Self {
        self.compile_operation_key = operation_key;
        self
    }
}

/// Draft execution inputs needed only when a regression suite compiles a template.
pub struct SkillRegressionCompileContext {
    /// Production tool router used to resolve governed capability registrations.
    pub router: Arc<ToolRouter>,
    /// Exact draft revision whose template is being reviewed.
    pub draft: StoredArtifactRevision,
    /// Files belonging to the exact draft revision.
    pub draft_files: Vec<ArtifactFile>,
}

/// Builds the review-time regression report for accepting a skill candidate.
pub async fn skill_acceptance_regression_report(
    config: MoaConfig,
    providers: Arc<ProviderRegistry>,
    registry: SkillRegistry,
    store: Arc<PostgresSessionStore>,
    scope: ActionRuleScope,
    candidate: LearningCandidate,
    compile_context: SkillRegressionCompileContext,
) -> Result<SkillRegressionGate> {
    // Review input is assembled from the artifact owner's contribution rows, not
    // from candidate payload JSON. The bytes are attributable storage: one
    // `generated` row for the candidate's own suite and one `accumulated` row per
    // deduped sibling session, each naming the session and experience it came
    // from so an erasure can reach them.
    let contributions = ArtifactRegistry::new(store.pool().clone())
        .list_suite_contributions(candidate.id)
        .await?;
    let Some(generated_suite) = generated_suite_contribution(&contributions) else {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "candidate has no generated regression suite",
                "generated_suite": null,
                "previous_skill": null,
            }),
            "candidate has no generated regression suite".to_string(),
        ));
    };

    let Some(skill_name) = skill_name(&candidate) else {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "candidate payload missing skill name",
                "generated_suite": generated_suite.summary(),
                "previous_skill": null,
            }),
            "candidate payload missing skill name".to_string(),
        ));
    };

    let previous_package = registry.load_package_by_name(&scope, &skill_name).await?;
    let previous_skill = previous_package
        .as_ref()
        .map(|package| previous_skill_payload(&package.skill));

    let mut suite = match toml::from_str::<TestSuite>(&generated_suite.suite_source) {
        Ok(suite) => suite,
        Err(error) => {
            return Ok(SkillRegressionGate::blocked(
                json!({
                    "regression_execution": "unavailable",
                    "runner": "moa-eval",
                    "reason": "generated regression suite could not be parsed",
                    "error": error.to_string(),
                    "generated_suite": generated_suite.summary(),
                    "previous_skill": previous_skill,
                }),
                "generated regression suite could not be parsed".to_string(),
            ));
        }
    };
    if suite.cases.is_empty() {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "generated regression suite contains no test cases",
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "generated regression suite contains no test cases".to_string(),
        ));
    }
    if suite.name.trim().is_empty() {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "generated regression suite has no name",
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "generated regression suite has no name".to_string(),
        ));
    }
    // A missing suite timeout parses as zero, which times every case out
    // instantly and rejects the candidate for a fixture defect rather than a
    // behavior regression. Floor it instead of trusting the TOML default.
    if suite.default_timeout_seconds == 0 {
        suite.default_timeout_seconds = DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS;
    }

    let candidate_package = SkillPackage::new(
        compile_context
            .draft_files
            .into_iter()
            .map(|file| SkillPackageFile {
                path: file.path,
                content: file.content,
                content_type: file.content_type,
                executable: file.executable,
            })
            .collect(),
    )
    .validate()?;
    let candidate_markdown = candidate_package.skill_md.clone();

    let definition = skill_definition_from_package(&candidate_package)?;
    let compile_operation_key = if let Some(template) = definition.execution_plan.as_ref() {
        let draft_revision_uid = draft_artifact_revision_uid(&candidate)?;
        let suite_hash = validated_suite_hash(&suite)?;
        let operation_key = format!("skill_regression:{draft_revision_uid}:{suite_hash}");
        let authority = resolve_skill_regression_compile_authority(
            store.pool().clone(),
            compile_context.router.capability_registrations(),
            scope,
            compile_context.draft,
        )
        .await?;
        let run_input = resolve_regression_execution_input(&suite)?;
        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &config,
            tenant_id: candidate.tenant_id,
            skill_name: &skill_name,
            skill_input_schema: &definition.inputs,
            template,
            run_input: &run_input,
            catalog: &authority.catalog,
            authorization: &authority.authorization,
            operation_key: &operation_key,
        })?;
        let audit_outcome = ExecutionRepository::new(store.pool().clone())
            .write_compile_audit(
                ExecutionScope::Tenant {
                    tenant_id: candidate.tenant_id,
                },
                &compiled.audit,
            )
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "skill regression compile audit persistence failed: {error}"
                ))
            })?;
        moa_brain::execution_planning::request::record_applied_planning_audit(&audit_outcome);
        if matches!(audit_outcome, CompileAuditWriteOutcome::Conflict { .. }) {
            return Err(MoaError::ValidationError(format!(
                "skill regression planning audit conflicts for operation `{operation_key}`"
            )));
        }
        if !compiled.accepted {
            return Ok(SkillRegressionGate::blocked(
                json!({
                    "regression_execution": "unavailable",
                    "runner": "moa-eval",
                    "reason": "candidate execution-plan template failed compilation",
                    "generated_suite": generated_suite.summary_with_suite(&suite),
                    "previous_skill": previous_skill,
                    "compile_operation_key": operation_key,
                }),
                "candidate execution-plan template failed compilation".to_string(),
            )
            .with_compile_operation_key(Some(operation_key)));
        }
        Some(operation_key)
    } else {
        None
    };

    // An unavailable provider is an operational failure, not a property of the
    // candidate: surface an error so the review can be retried after the
    // deployment is fixed, instead of silently waiving the gate.
    let provider =
        providers.provider_for_model(Some(config.model_for_task(ModelTask::MainLoop)))?;

    // Held-out material the candidate was not derived from: the previous
    // promoted revision's own suite (it rode that revision's package) plus
    // sibling suites accumulated from deduped recurring sessions.
    let held_out = collect_held_out_pool(previous_package.as_ref(), &contributions);

    let run_count = if previous_package.is_some() { 2.0 } else { 1.0 };
    let mut estimated_cost = estimate_suite_cost(&suite, provider.as_ref()) * run_count;
    if let Some(pool_suite) = &held_out.suite {
        estimated_cost += estimate_suite_cost(pool_suite, provider.as_ref()) * run_count;
    }
    if estimated_cost > DEFAULT_SKILL_TEST_BUDGET_DOLLARS {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "estimated regression cost exceeds budget",
                "estimated_cost_dollars": estimated_cost,
                "budget_dollars": DEFAULT_SKILL_TEST_BUDGET_DOLLARS,
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "estimated regression cost exceeds the review budget".to_string(),
        )
        .with_compile_operation_key(compile_operation_key));
    }

    let Some(previous_package) = previous_package else {
        // First revision of a new skill: nothing to compare against, so the
        // candidate suite runs alone as a smoke gate instead of being skipped,
        // and any sibling suites run as true held-out material.
        let candidate_run = execute_candidate_only(
            config.clone(),
            suite.clone(),
            skill_name.clone(),
            candidate_markdown.clone(),
            provider.clone(),
        )
        .await?;
        let candidate_summary = summarize_regression_run(&candidate_run);
        let has_execution_failure = run_has_execution_failure(&candidate_run);
        let held_in_accepted = !has_execution_failure && candidate_summary.failed_runs == 0;

        let mut held_out_report = held_out.report_base();
        let mut held_out_accepted = true;
        if let Some(pool_suite) = &held_out.suite {
            let pool_run = execute_candidate_only(
                config,
                pool_suite.clone(),
                skill_name,
                candidate_markdown,
                provider,
            )
            .await?;
            let pool_summary = summarize_regression_run(&pool_run);
            // Sibling suites come from resolved sessions of the same task, so
            // the candidate is expected to pass them outright.
            held_out_accepted = pool_summary.failed_runs == 0;
            held_out_report["decision"] = json!(if held_out_accepted {
                "accepted"
            } else {
                "rejected"
            });
            held_out_report["candidate"] = regression_summary_to_json(&pool_summary);
            held_out_report["candidate_failures"] = run_failures_json(&pool_run);
        }

        let accepted = held_in_accepted && held_out_accepted;
        let report = json!({
            "regression_execution": "completed",
            "execution_mode": SkillRegressionExecution::CandidateOnly.as_str(),
            "runner": "moa-eval",
            "decision": if has_execution_failure {
                "eval_failed"
            } else if accepted {
                "accepted"
            } else {
                "rejected"
            },
            "generated_suite": generated_suite.summary_with_suite(&suite),
            "previous_skill": null,
            "candidate": regression_summary_to_json(&candidate_summary),
            "candidate_failures": run_failures_json(&candidate_run),
            "held_out": held_out_report,
        });
        return Ok(if accepted {
            SkillRegressionGate::accepted(
                report,
                SkillRegressionExecution::CandidateOnly,
                held_out.source_count,
            )
            .with_compile_operation_key(compile_operation_key)
        } else {
            SkillRegressionGate {
                report,
                allow_promotion: false,
                rejection_reason: Some(if has_execution_failure {
                    "skill regression eval failed".to_string()
                } else if !held_in_accepted {
                    "candidate skill failed its generated regression suite".to_string()
                } else {
                    "candidate skill failed the held-out sibling suites".to_string()
                }),
                execution: SkillRegressionExecution::Blocked,
                held_out_sources: held_out.source_count,
                compile_operation_key,
            }
        });
    };
    let previous_markdown = previous_package.skill_markdown()?.to_string();

    let executed = execute_previous_and_candidate(
        config.clone(),
        suite.clone(),
        skill_name.clone(),
        previous_markdown.clone(),
        candidate_markdown.clone(),
        provider.clone(),
    )
    .await?;

    let previous_summary = summarize_regression_run(&executed.previous);
    let candidate_summary = summarize_regression_run(&executed.candidate);
    let has_execution_failure = run_has_execution_failure(&executed.previous)
        || run_has_execution_failure(&executed.candidate);
    let held_in_accepted =
        !has_execution_failure && compare_scores(&previous_summary, &candidate_summary);

    let mut held_out_report = held_out.report_base();
    let mut held_out_accepted = true;
    if let Some(pool_suite) = &held_out.suite {
        let pool = execute_previous_and_candidate(
            config,
            pool_suite.clone(),
            skill_name,
            previous_markdown,
            candidate_markdown,
            provider,
        )
        .await?;
        let pool_previous = summarize_regression_run(&pool.previous);
        let pool_candidate = summarize_regression_run(&pool.candidate);
        // No separate execution-failure rejection here: a stale pooled case
        // that errors for environmental reasons fails both runs equally and
        // the comparison neutralizes it. Only the candidate doing worse than
        // the previous revision on material it never saw is a regression.
        held_out_accepted = compare_scores(&pool_previous, &pool_candidate);
        held_out_report["decision"] = json!(if held_out_accepted {
            "accepted"
        } else {
            "rejected"
        });
        held_out_report["previous"] = regression_summary_to_json(&pool_previous);
        held_out_report["previous_failures"] = run_failures_json(&pool.previous);
        held_out_report["candidate"] = regression_summary_to_json(&pool_candidate);
        held_out_report["candidate_failures"] = run_failures_json(&pool.candidate);
    }

    let accepted = held_in_accepted && held_out_accepted;
    let report = json!({
        "regression_execution": "completed",
        "execution_mode": SkillRegressionExecution::ComparedWithPrevious.as_str(),
        "runner": "moa-eval",
        "decision": if has_execution_failure {
            "eval_failed"
        } else if accepted {
            "accepted"
        } else {
            "rejected"
        },
        "generated_suite": generated_suite.summary_with_suite(&suite),
        "previous_skill": previous_skill_payload(&previous_package.skill),
        "previous": regression_summary_to_json(&previous_summary),
        "previous_failures": run_failures_json(&executed.previous),
        "candidate": regression_summary_to_json(&candidate_summary),
        "candidate_failures": run_failures_json(&executed.candidate),
        "held_out": held_out_report,
    });

    if accepted {
        Ok(SkillRegressionGate::accepted(
            report,
            SkillRegressionExecution::ComparedWithPrevious,
            held_out.source_count,
        )
        .with_compile_operation_key(compile_operation_key))
    } else {
        Ok(SkillRegressionGate {
            report,
            allow_promotion: false,
            rejection_reason: Some(if has_execution_failure {
                "skill regression eval failed".to_string()
            } else if !held_in_accepted {
                "skill regression rejected the proposed draft".to_string()
            } else {
                "candidate regressed on the held-out suite pool".to_string()
            }),
            execution: SkillRegressionExecution::Blocked,
            held_out_sources: held_out.source_count,
            compile_operation_key,
        })
    }
}

/// Held-out evaluation material pooled for one gate run.
struct HeldOutPool {
    /// Merged pool suite, when any source contributed cases.
    suite: Option<TestSuite>,
    /// Number of distinct suite sources pooled.
    source_count: usize,
    /// Pool entries skipped with the reason (for report honesty).
    skipped: Vec<String>,
}

impl HeldOutPool {
    /// Base report object describing the pool before any execution results.
    fn report_base(&self) -> Value {
        json!({
            "source_count": self.source_count,
            "case_count": self
                .suite
                .as_ref()
                .map(|suite| suite.cases.len())
                .unwrap_or(0),
            "skipped": self.skipped,
            "decision": if self.suite.is_some() { "pending" } else { "no_material" },
        })
    }
}

/// Pools held-out suites: the previous promoted revision's own suite plus any
/// sibling suites accumulated onto the candidate from deduped sessions.
///
/// Sources that fail to parse are skipped with a recorded reason rather than
/// rejecting the candidate — pool corruption is not a property of the draft
/// under review. Case names are prefixed by source so merged cases stay unique.
fn collect_held_out_pool(
    previous_package: Option<&moa_skills::registry::StoredSkillPackage>,
    contributions: &[StoredSuiteContribution],
) -> HeldOutPool {
    let mut cases = Vec::new();
    let mut source_count = 0usize;
    let mut skipped = Vec::new();

    if let Some(file) = previous_package.and_then(|package| {
        package
            .files
            .iter()
            .find(|file| file.path == moa_skills::regression::REGRESSION_SUITE_PACKAGE_PATH)
    }) {
        match std::str::from_utf8(&file.content)
            .map_err(|error| error.to_string())
            .and_then(|text| toml::from_str::<TestSuite>(text).map_err(|error| error.to_string()))
        {
            Ok(suite) => {
                source_count += 1;
                cases.extend(prefixed_cases("prev", suite));
            }
            Err(error) => skipped.push(format!("previous revision suite unreadable: {error}")),
        }
    }

    for (index, contribution) in contributions
        .iter()
        .filter(|contribution| contribution.kind == SuiteContributionKind::Accumulated)
        .enumerate()
    {
        match toml::from_str::<TestSuite>(&contribution.suite_source) {
            Ok(suite) => {
                source_count += 1;
                cases.extend(prefixed_cases(&format!("sib{index}"), suite));
            }
            Err(error) => skipped.push(format!(
                "sibling suite `{}` unreadable: {error}",
                contribution.suite_name
            )),
        }
    }

    let suite = (!cases.is_empty()).then(|| TestSuite {
        name: "held-out-pool".to_string(),
        description: Some(
            "Pooled held-out suites from prior revisions and sibling sessions".to_string(),
        ),
        cases,
        default_timeout_seconds: DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS,
        tags: vec!["skill".to_string(), "held-out".to_string()],
    });
    HeldOutPool {
        suite,
        source_count,
        skipped,
    }
}

/// Prefixes pooled case names by source so merged cases stay unique.
fn prefixed_cases(
    prefix: &str,
    suite: TestSuite,
) -> impl Iterator<Item = moa_eval_core::TestCase> + '_ {
    suite.cases.into_iter().map(move |mut case| {
        case.name = format!("{prefix}-{}", case.name);
        case
    })
}

/// Suite source format; generated suites are always TOML.
const GENERATED_SUITE_SOURCE_FORMAT: &str = "toml";

/// Report fields describing the candidate's own generated suite.
trait GeneratedSuiteReport {
    fn summary(&self) -> Value;
    fn summary_with_suite(&self, suite: &TestSuite) -> Value;
}

impl GeneratedSuiteReport for StoredSuiteContribution {
    fn summary(&self) -> Value {
        json!({
            "relative_path": self.suite_name,
            "source_format": GENERATED_SUITE_SOURCE_FORMAT,
            "source_text_present": true,
        })
    }

    fn summary_with_suite(&self, suite: &TestSuite) -> Value {
        json!({
            "relative_path": self.suite_name,
            "source_format": GENERATED_SUITE_SOURCE_FORMAT,
            "source_text_present": true,
            "suite_name": suite.name,
            "case_count": suite.cases.len(),
        })
    }
}

/// Returns the candidate's own generated suite from its contribution rows.
fn generated_suite_contribution(
    contributions: &[StoredSuiteContribution],
) -> Option<&StoredSuiteContribution> {
    contributions
        .iter()
        .find(|contribution| contribution.kind == SuiteContributionKind::Generated)
}

fn skill_name(candidate: &LearningCandidate) -> Option<String> {
    candidate
        .payload
        .get("artifact_name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| candidate.target_label.clone())
}

#[derive(Debug, Clone, PartialEq)]
enum RegressionExecutionInput {
    Missing,
    Resolved(Value),
    Ambiguous,
}

fn resolve_regression_execution_input(suite: &TestSuite) -> Result<RegressionExecutionInput> {
    let mut canonical_inputs = Vec::new();
    for input in suite
        .cases
        .iter()
        .filter_map(|case| case.metadata.get(EXECUTION_INPUT_METADATA_KEY))
    {
        let canonical = artifact_canonical_json_bytes(input)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        if canonical_inputs
            .iter()
            .any(|(existing, _)| existing == &canonical)
        {
            continue;
        }
        canonical_inputs.push((canonical, input.clone()));
    }

    match canonical_inputs.as_slice() {
        [] => Ok(RegressionExecutionInput::Missing),
        [(_, input)] => Ok(RegressionExecutionInput::Resolved(input.clone())),
        _ => Ok(RegressionExecutionInput::Ambiguous),
    }
}

struct ExecutedRegressionRuns {
    previous: EvalRun,
    candidate: EvalRun,
}

async fn execute_candidate_only(
    config: MoaConfig,
    suite: TestSuite,
    skill_name: String,
    candidate_markdown: String,
    provider: Arc<dyn LLMProvider>,
) -> Result<EvalRun> {
    let join = tokio::task::spawn_blocking(move || {
        block_on_current_thread(Box::pin(execute_skill_suite(
            config,
            suite,
            skill_name,
            candidate_markdown,
            provider,
            "candidate".to_string(),
        )))
    })
    .await
    .map_err(|error| MoaError::StorageError(error.to_string()))?;
    join.map_err(MoaError::StorageError)?
}

async fn execute_previous_and_candidate(
    config: MoaConfig,
    suite: TestSuite,
    skill_name: String,
    previous_markdown: String,
    candidate_markdown: String,
    provider: Arc<dyn LLMProvider>,
) -> Result<ExecutedRegressionRuns> {
    let join = tokio::task::spawn_blocking(move || {
        block_on_current_thread(Box::pin(async move {
            let previous = execute_skill_suite(
                config.clone(),
                suite.clone(),
                skill_name.clone(),
                previous_markdown,
                provider.clone(),
                "previous".to_string(),
            )
            .await?;
            let candidate = execute_skill_suite(
                config,
                suite,
                skill_name,
                candidate_markdown,
                provider,
                "candidate".to_string(),
            )
            .await?;
            Ok(ExecutedRegressionRuns {
                previous,
                candidate,
            })
        }))
    })
    .await
    .map_err(|error| MoaError::StorageError(error.to_string()))?;
    join.map_err(MoaError::StorageError)?
}

fn block_on_current_thread<T>(future: LocalBoxFuture<T>) -> std::result::Result<T, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    Ok(runtime.block_on(future))
}

async fn execute_skill_suite(
    config: MoaConfig,
    suite: TestSuite,
    skill_name: String,
    skill_markdown: String,
    provider: Arc<dyn LLMProvider>,
    label: String,
) -> Result<EvalRun> {
    let agent_config = skill_agent_config(&skill_name, &skill_markdown, &label);
    let engine = EvalEngine::new(
        config,
        EngineOptions {
            parallel: 1,
            temp_dir: std::env::temp_dir().join("moa-eval-skill-review"),
            ..EngineOptions::default()
        },
    )
    .map_err(map_eval_error)?;
    let mut run = engine
        .run_suite_with_provider(&suite, std::slice::from_ref(&agent_config), provider)
        .await
        .map_err(map_eval_error)?;
    let evaluators = default_skill_evaluators()?;
    evaluate_run(&suite, &mut run, &evaluators)
        .await
        .map_err(map_eval_error)?;
    Ok(run)
}

/// Builds the eval agent whose only difference between the previous and
/// candidate runs is the skill revision under test.
///
/// The skill rides `system_prompt_append` because that is the surface the eval
/// pipeline actually consumes (`compose_identity_prompt`); the two runs must
/// differ by exactly the skill content or the score comparison is vacuous.
fn skill_agent_config(skill_name: &str, skill_markdown: &str, label: &str) -> AgentConfig {
    AgentConfig {
        name: format!("skill-{skill_name}-{label}"),
        instructions: InstructionOverride {
            system_prompt_append: Some(format!(
                "## Active skill: {skill_name}\n\n\
                 Apply the following skill instructions when they match the task.\n\n\
                 {skill_markdown}"
            )),
            ..InstructionOverride::default()
        },
        permissions: ActionPolicyOverride::default(),
        ..AgentConfig::default()
    }
}

fn default_skill_evaluators() -> Result<Vec<Box<dyn Evaluator>>> {
    let names = DEFAULT_SKILL_EVALUATORS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    build_evaluators(&names, &EvaluatorOptions::default()).map_err(map_eval_error)
}

fn summarize_regression_run(run: &EvalRun) -> SkillRegressionSummary {
    let total_runs = run.results.len();
    let failed_runs = run
        .results
        .iter()
        .filter(|result| !matches!(result.status, EvalStatus::Passed | EvalStatus::Skipped))
        .count();
    let average_score = if run.results.is_empty() {
        1.0
    } else {
        run.results.iter().map(result_score).sum::<f64>() / run.results.len() as f64
    };

    SkillRegressionSummary {
        average_score,
        failed_runs,
        total_runs,
        total_cost_dollars: run.summary.total_cost_dollars,
    }
}

fn result_score(result: &EvalResult) -> f64 {
    if result.scores.is_empty() {
        return match result.status {
            EvalStatus::Passed | EvalStatus::Skipped => 1.0,
            EvalStatus::Failed | EvalStatus::Error | EvalStatus::Timeout => 0.0,
        };
    }

    let mut total = 0.0;
    let mut count = 0usize;
    for score in &result.scores {
        match &score.value {
            EvalScoreValue::Numeric(value) => {
                total += *value;
                count += 1;
            }
            EvalScoreValue::Boolean(value) => {
                total += if *value { 1.0 } else { 0.0 };
                count += 1;
            }
            EvalScoreValue::Categorical(_) => {}
        }
    }

    if count == 0 {
        1.0
    } else {
        total / count as f64
    }
}

fn run_has_execution_failure(run: &EvalRun) -> bool {
    run.results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Error | EvalStatus::Timeout))
}

fn estimate_suite_cost(suite: &TestSuite, llm: &dyn LLMProvider) -> f64 {
    let pricing = llm.capabilities().pricing;
    suite
        .cases
        .iter()
        .map(|case| {
            let prompt_tokens = estimate_tokens(&case.input).max(128);
            let output_tokens = llm.capabilities().max_output.clamp(256, 2_048);
            ((prompt_tokens as f64 * pricing.input_per_mtok)
                + (output_tokens as f64 * pricing.output_per_mtok))
                / 1_000_000.0
        })
        .sum()
}

fn previous_skill_payload(skill: &moa_skills::registry::Skill) -> Value {
    json!({
        "skill_uid": skill.skill_uid,
        "version": skill.version,
        "name": skill.name,
    })
}

fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

struct SkillTemplateCompileRequest<'a> {
    config: &'a MoaConfig,
    tenant_id: TenantId,
    skill_name: &'a str,
    skill_input_schema: &'a Value,
    template: &'a ExecutionPlanTemplate,
    run_input: &'a RegressionExecutionInput,
    catalog: &'a ExecutionCapabilityCatalog,
    authorization: &'a ExecutionAuthorizationEnvelope,
    operation_key: &'a str,
}

struct SkillTemplateCompile {
    audit: ExecutionPlanningAuditEnvelope,
    accepted: bool,
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

fn compile_skill_execution_template(
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
    let candidate_bytes = artifact_canonical_json_bytes(&candidate)
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
    let bytes = artifact_canonical_json_bytes(&report)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    String::from_utf8(bytes).map_err(|error| MoaError::SerializationError(error.to_string()))
}

fn draft_artifact_revision_uid(candidate: &LearningCandidate) -> Result<uuid::Uuid> {
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

fn validated_suite_hash(suite: &TestSuite) -> Result<String> {
    let bytes = artifact_canonical_json_bytes(&json!({
        "schema_version": 1,
        "suite": suite,
    }))
    .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Collects per-case failure detail so a rejected candidate's report explains
/// what actually failed instead of only carrying aggregate counts.
fn run_failures_json(run: &EvalRun) -> Value {
    let failures = run
        .results
        .iter()
        .filter(|result| !matches!(result.status, EvalStatus::Passed | EvalStatus::Skipped))
        .map(|result| {
            json!({
                "test_case": result.test_case,
                "status": format!("{:?}", result.status),
                "error": result.error,
            })
        })
        .collect::<Vec<_>>();
    Value::Array(failures)
}

fn regression_summary_to_json(summary: &SkillRegressionSummary) -> Value {
    json!({
        "average_score": summary.average_score,
        "failed_runs": summary.failed_runs,
        "total_runs": summary.total_runs,
        "total_cost_dollars": summary.total_cost_dollars,
    })
}

fn map_eval_error(error: moa_eval_core::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use moa_artifacts::execution_plan::ExecutionPlanTemplate;
    use moa_config::MoaConfig;
    use moa_core::types::{
        execution_planning::{
            ExecutionAuditReport, ExecutionCompileOutcome, ExecutionCompileSource,
            ExecutionPlanningAuditPayload,
        },
        identifiers::TenantId,
    };
    use moa_execution::{ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog};
    use moa_hands::ToolRegistry;
    use serde_json::json;

    use crate::services::execution::build_capability_response;

    use super::{
        RegressionExecutionInput, SkillTemplateCompileRequest, StoredSuiteContribution,
        SuiteContributionKind, collect_held_out_pool, compile_skill_execution_template,
        generated_suite_contribution, resolve_regression_execution_input,
    };

    fn contribution(
        kind: SuiteContributionKind,
        suite_name: &str,
        suite_source: &str,
    ) -> StoredSuiteContribution {
        StoredSuiteContribution {
            kind,
            suite_name: suite_name.to_string(),
            suite_source: suite_source.to_string(),
            source_session_id: Some(uuid::Uuid::now_v7()),
            source_experience_id: Some(uuid::Uuid::now_v7()),
        }
    }

    #[test]
    fn held_out_pool_merges_sibling_suites_with_prefixed_case_names() {
        // Pins: accumulated sibling suites merge into one pool suite with source-prefixed
        // case names, and unreadable entries are skipped with a recorded reason instead
        // of rejecting the candidate.
        let contributions = [
            contribution(
                SuiteContributionKind::Accumulated,
                "sibling/a",
                "[suite]\nname = \"s0\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"smoke\"\ninput = \"run\"\n",
            ),
            contribution(
                SuiteContributionKind::Accumulated,
                "sibling/b",
                "this is [not toml",
            ),
        ];

        let pool = collect_held_out_pool(None, &contributions);

        assert_eq!(pool.source_count, 1);
        assert_eq!(
            pool.skipped.len(),
            1,
            "unreadable sibling is recorded, not fatal"
        );
        let suite = pool.suite.expect("readable sibling contributes cases");
        assert_eq!(suite.cases.len(), 1);
        assert_eq!(suite.cases[0].name, "sib0-smoke");
    }

    #[test]
    fn held_out_pool_excludes_the_candidates_own_generated_suite() {
        // Pins: the pool is HELD-OUT material. The candidate's own generated suite lives
        // in the same contribution table as its siblings, so a collector that took every
        // row would grade the draft on the very cases it was derived from and report a
        // passing held-out split that never held anything out.
        let contributions = [
            contribution(
                SuiteContributionKind::Generated,
                "tests/regression-suite.toml",
                "[suite]\nname = \"own\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"own\"\ninput = \"run\"\n",
            ),
            contribution(
                SuiteContributionKind::Accumulated,
                "sibling/a",
                "[suite]\nname = \"s0\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"smoke\"\ninput = \"run\"\n",
            ),
        ];

        let pool = collect_held_out_pool(None, &contributions);

        assert_eq!(
            pool.source_count, 1,
            "only the sibling is held-out material"
        );
        let suite = pool.suite.expect("sibling contributes cases");
        assert_eq!(suite.cases.len(), 1);
        assert_eq!(suite.cases[0].name, "sib0-smoke");
    }

    #[test]
    fn the_generated_suite_is_selected_by_kind_not_by_position() {
        // Pins: the candidate's own suite is identified by its stored kind. Rows come back
        // ordered by `suite_kind` first, so a positional read would silently pick an
        // accumulated sibling as the candidate's own suite whenever the generated row was
        // erased — grading a draft against another session's cases.
        let contributions = [
            contribution(SuiteContributionKind::Accumulated, "sibling/a", "x"),
            contribution(
                SuiteContributionKind::Generated,
                "tests/regression-suite.toml",
                "y",
            ),
        ];

        let generated =
            generated_suite_contribution(&contributions).expect("generated suite is found");
        assert_eq!(generated.suite_name, "tests/regression-suite.toml");
        assert!(generated_suite_contribution(&contributions[..1]).is_none());
    }

    #[test]
    fn held_out_pool_is_empty_without_material() {
        // Pins: a first revision of a novel task has no held-out material and the report
        // base says so instead of implying a split ran.
        let pool = collect_held_out_pool(None, &[]);

        assert_eq!(pool.source_count, 0);
        assert!(pool.suite.is_none());
        assert_eq!(pool.report_base()["decision"], "no_material");
    }

    #[test]
    fn regression_template_input_comes_only_from_explicit_structured_case_metadata() {
        // Pins: free-form case input is never parsed as template JSON; one explicit
        // structured metadata value is preserved as compiler input, while no value stays missing.
        let exact = json!({"ticket": "INC-42", "options": {"notify": true}});
        let suite = moa_eval_core::TestSuite {
            cases: vec![
                moa_eval_core::TestCase {
                    input: r#"{"ticket":"fabricated-from-prose"}"#.to_string(),
                    ..moa_eval_core::TestCase::default()
                },
                moa_eval_core::TestCase {
                    metadata: std::collections::HashMap::from([(
                        "execution_input".to_string(),
                        exact.clone(),
                    )]),
                    ..moa_eval_core::TestCase::default()
                },
            ],
            ..moa_eval_core::TestSuite::default()
        };

        assert_eq!(
            resolve_regression_execution_input(&suite)
                .expect("one structured input should resolve"),
            RegressionExecutionInput::Resolved(exact)
        );
        assert_eq!(
            resolve_regression_execution_input(&moa_eval_core::TestSuite {
                cases: vec![moa_eval_core::TestCase {
                    input: r#"{"ticket":"still-prose"}"#.to_string(),
                    ..moa_eval_core::TestCase::default()
                }],
                ..moa_eval_core::TestSuite::default()
            })
            .expect("free-form input is ignored"),
            RegressionExecutionInput::Missing
        );
    }

    #[test]
    fn canonical_identical_regression_execution_inputs_resolve_once() {
        // Pins: semantically identical explicit objects deduplicate through artifact
        // canonical JSON even when their source key insertion order differs.
        let first: serde_json::Value =
            serde_json::from_str(r#"{"ticket":"INC-42","options":{"notify":true,"limit":2}}"#)
                .expect("first structured input parses");
        let reordered: serde_json::Value =
            serde_json::from_str(r#"{"options":{"limit":2,"notify":true},"ticket":"INC-42"}"#)
                .expect("reordered structured input parses");
        let suite = moa_eval_core::TestSuite {
            cases: vec![
                moa_eval_core::TestCase {
                    metadata: std::collections::HashMap::from([(
                        "execution_input".to_string(),
                        first.clone(),
                    )]),
                    ..moa_eval_core::TestCase::default()
                },
                moa_eval_core::TestCase {
                    metadata: std::collections::HashMap::from([(
                        "execution_input".to_string(),
                        reordered,
                    )]),
                    ..moa_eval_core::TestCase::default()
                },
            ],
            ..moa_eval_core::TestSuite::default()
        };

        assert_eq!(
            resolve_regression_execution_input(&suite)
                .expect("canonical-identical inputs should resolve"),
            RegressionExecutionInput::Resolved(first)
        );
    }

    #[test]
    fn distinct_regression_execution_inputs_need_input_and_cannot_be_accepted() {
        // Pins: a suite with conflicting explicit template inputs is ambiguous and must
        // produce a typed NeedsInput audit instead of silently compiling the first case.
        let suite = moa_eval_core::TestSuite {
            cases: vec![
                moa_eval_core::TestCase {
                    metadata: std::collections::HashMap::from([(
                        "execution_input".to_string(),
                        json!({"ticket": "INC-42"}),
                    )]),
                    ..moa_eval_core::TestCase::default()
                },
                moa_eval_core::TestCase {
                    metadata: std::collections::HashMap::from([(
                        "execution_input".to_string(),
                        json!({"ticket": "INC-43"}),
                    )]),
                    ..moa_eval_core::TestCase::default()
                },
            ],
            ..moa_eval_core::TestSuite::default()
        };
        let mut template = output_only_template();
        template.plan.input_schema = json!({
            "type": "object",
            "required": ["ticket"],
            "properties": {"ticket": {"type": "string"}},
            "additionalProperties": false
        });
        let skill_input_schema = template.plan.input_schema.clone();
        let run_input = resolve_regression_execution_input(&suite)
            .expect("conflicting structured inputs produce a typed resolution");
        assert_eq!(run_input, RegressionExecutionInput::Ambiguous);
        let (catalog, authorization) = empty_authority("input-template");
        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &MoaConfig::default(),
            tenant_id: TenantId::new(),
            skill_name: "input-template",
            skill_input_schema: &skill_input_schema,
            template: &template,
            run_input: &run_input,
            catalog: &catalog,
            authorization: &authorization,
            operation_key: &format!(
                "skill_regression:{}:{}",
                uuid::Uuid::now_v7(),
                "f".repeat(64)
            ),
        })
        .expect("input ambiguity is captured in a strict compile audit");

        assert!(!compiled.accepted, "ambiguous input must never be accepted");
        assert_eq!(
            compile_outcome_and_candidate_hash(&compiled).0,
            ExecutionCompileOutcome::NeedsInput
        );
        let ExecutionPlanningAuditPayload::Compile {
            validation_report,
            final_plan_hash,
            ..
        } = &compiled.audit.payload
        else {
            panic!("input ambiguity must emit a compile audit");
        };
        assert_eq!(final_plan_hash, &None);
        let ExecutionAuditReport::Compiler { violations, .. } =
            serde_json::from_str(validation_report).expect("strict compiler report parses")
        else {
            panic!("input ambiguity must emit a compiler report");
        };
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "ambiguous_run_input");
        assert_eq!(violations[0].path, "run_input");
    }

    fn output_only_template() -> ExecutionPlanTemplate {
        serde_json::from_value(json!({
            "goal": {
                "requirements": [{
                    "id": "regression_result",
                    "description": "Return the deterministic regression result."
                }],
                "deliverables": [],
                "coverage": [],
                "constraints": [],
                "completion_checks": [{
                    "id": "output_schema",
                    "description": "Validate the regression output.",
                    "requirement_ids": ["regression_result"],
                    "constraint_ids": [],
                    "kind": {"kind": "output_schema"}
                }]
            },
            "plan": {
                "schema_version": 1,
                "input_schema": {
                    "type": "object",
                    "additionalProperties": false
                },
                "output_schema": {"type": "object"},
                "nodes": [{
                    "id": "result",
                    "requirement_ids": ["regression_result"],
                    "depends_on": [],
                    "when": null,
                    "input": {},
                    "output_schema": {"type": "object"},
                    "operation": {
                        "kind": "output",
                        "value": {"status": "validated"}
                    },
                    "retry": {
                        "max_attempts": 1,
                        "initial_backoff_ms": 0,
                        "max_backoff_ms": 0
                    },
                    "budget": null
                }]
            }
        }))
        .expect("execution-plan template parses")
    }

    fn empty_authority(
        skill_name: &str,
    ) -> (ExecutionCapabilityCatalog, ExecutionAuthorizationEnvelope) {
        let catalog = ExecutionCapabilityCatalog::build(Vec::new())
            .expect("empty governed catalog should build");
        let authorization = ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: vec![moa_artifacts::reference::ArtifactRef::artifact(
                moa_artifacts::document::ArtifactKind::Skill,
                skill_name,
            )],
        };
        (catalog, authorization)
    }

    fn compile_outcome_and_candidate_hash(
        compiled: &super::SkillTemplateCompile,
    ) -> (ExecutionCompileOutcome, String) {
        let ExecutionPlanningAuditPayload::Compile {
            outcome,
            candidate_hash,
            ..
        } = &compiled.audit.payload
        else {
            panic!("template compilation must emit a compile audit");
        };
        (*outcome, candidate_hash.clone())
    }

    fn governed_capability_template(
        reference: &moa_artifacts::execution_plan::CapabilityReference,
        output_schema: &serde_json::Value,
    ) -> ExecutionPlanTemplate {
        serde_json::from_value(json!({
            "goal": {
                "requirements": [{
                    "id": "regression_result",
                    "description": "Read the governed regression fixture."
                }],
                "deliverables": [],
                "coverage": [],
                "constraints": [],
                "completion_checks": [{
                    "id": "output_schema",
                    "description": "Validate the regression output.",
                    "requirement_ids": ["regression_result"],
                    "constraint_ids": [],
                    "kind": {"kind": "output_schema"}
                }]
            },
            "plan": {
                "schema_version": 1,
                "input_schema": {
                    "type": "object",
                    "additionalProperties": false
                },
                "output_schema": output_schema,
                "nodes": [
                    {
                        "id": "read_fixture",
                        "requirement_ids": ["regression_result"],
                        "depends_on": [],
                        "when": null,
                        "input": {"path": "SKILL.md"},
                        "output_schema": output_schema,
                        "operation": {
                            "kind": "capability",
                            "reference": reference
                        },
                        "retry": {
                            "max_attempts": 1,
                            "initial_backoff_ms": 0,
                            "max_backoff_ms": 0
                        },
                        "budget": null
                    },
                    {
                        "id": "result",
                        "requirement_ids": ["regression_result"],
                        "depends_on": ["read_fixture"],
                        "when": null,
                        "input": {},
                        "output_schema": output_schema,
                        "operation": {
                            "kind": "output",
                            "value": {"$ref": "$.nodes.read_fixture.output"}
                        },
                        "retry": {
                            "max_attempts": 1,
                            "initial_backoff_ms": 0,
                            "max_backoff_ms": 0
                        },
                        "budget": null
                    }
                ]
            }
        }))
        .expect("governed capability template parses")
    }

    #[test]
    fn execution_template_uses_the_execution_compiler_and_emits_strict_audit() {
        // Pins: a template-bearing draft is validated by the shared execution compiler and
        // produces the sessionless skill-regression audit consumed by the learning append.
        let config = MoaConfig::default();
        let tenant_id = TenantId::new();
        let operation_key = format!(
            "skill_regression:{}:{}",
            uuid::Uuid::now_v7(),
            "a".repeat(64)
        );
        let run_input = json!({});
        let run_input = RegressionExecutionInput::Resolved(run_input);
        let (catalog, authorization) = empty_authority("regression-template");
        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &config,
            tenant_id,
            skill_name: "regression-template",
            skill_input_schema: &json!({
                "type": "object",
                "additionalProperties": false
            }),
            template: &output_only_template(),
            run_input: &run_input,
            catalog: &catalog,
            authorization: &authorization,
            operation_key: &operation_key,
        })
        .expect("valid output-only template compiles");

        assert!(compiled.accepted);
        assert_eq!(compiled.audit.tenant_id, tenant_id);
        assert_eq!(compiled.audit.contact_id, None);
        assert_eq!(compiled.audit.session_id, None);
        assert_eq!(compiled.audit.originating_sequence, None);
        let ExecutionPlanningAuditPayload::Compile {
            source,
            operation_key: persisted_key,
            outcome,
            candidate_hash,
            final_plan_hash,
            validation_report,
            ..
        } = compiled.audit.payload
        else {
            panic!("template compilation must emit a compile audit");
        };
        assert_eq!(source, ExecutionCompileSource::SkillRegression);
        assert_eq!(persisted_key, operation_key);
        assert_eq!(outcome, ExecutionCompileOutcome::Accepted);
        assert_eq!(candidate_hash.len(), 64);
        assert!(final_plan_hash.is_some());
        assert!(matches!(
            serde_json::from_str::<ExecutionAuditReport>(&validation_report)
                .expect("compiler report stays strict"),
            ExecutionAuditReport::Compiler { .. }
        ));
    }

    #[test]
    fn execution_template_compiles_with_exact_supplied_structured_input() {
        // Pins: the regression compiler consumes the explicit structured input instead of
        // fabricating an empty object; changing only that input changes the audited candidate.
        let mut template = output_only_template();
        template.plan.input_schema = json!({
            "type": "object",
            "required": ["ticket"],
            "properties": {"ticket": {"type": "string"}},
            "additionalProperties": false
        });
        let skill_input_schema = json!({
            "type": "object",
            "required": ["ticket"],
            "properties": {"ticket": {"type": "string"}},
            "additionalProperties": false
        });
        let first_input = json!({"ticket": "INC-42"});
        let second_input = json!({"ticket": "INC-43"});
        let (catalog, authorization) = empty_authority("input-template");
        let compile_with_input = |run_input: &serde_json::Value| {
            let run_input = RegressionExecutionInput::Resolved(run_input.clone());
            compile_skill_execution_template(SkillTemplateCompileRequest {
                config: &MoaConfig::default(),
                tenant_id: TenantId::new(),
                skill_name: "input-template",
                skill_input_schema: &skill_input_schema,
                template: &template,
                run_input: &run_input,
                catalog: &catalog,
                authorization: &authorization,
                operation_key: &format!(
                    "skill_regression:{}:{}",
                    uuid::Uuid::now_v7(),
                    "b".repeat(64)
                ),
            })
            .expect("valid structured regression input compiles")
        };

        let first = compile_with_input(&first_input);
        let second = compile_with_input(&second_input);
        let (first_outcome, first_hash) = compile_outcome_and_candidate_hash(&first);
        let (second_outcome, second_hash) = compile_outcome_and_candidate_hash(&second);

        assert!(first.accepted);
        assert!(second.accepted);
        assert_eq!(first_outcome, ExecutionCompileOutcome::Accepted);
        assert_eq!(second_outcome, ExecutionCompileOutcome::Accepted);
        assert_ne!(
            first_hash, second_hash,
            "candidate hash must bind exact input"
        );
    }

    #[test]
    fn execution_template_with_missing_or_invalid_structured_input_needs_input() {
        // Pins: absence and schema-invalid structured input remain typed input failures.
        let mut template = output_only_template();
        template.plan.input_schema = json!({
            "type": "object",
            "required": ["ticket"],
            "properties": {"ticket": {"type": "string"}},
            "additionalProperties": false
        });
        let skill_input_schema = template.plan.input_schema.clone();
        let invalid_input = json!({"ticket": 42});
        let (catalog, authorization) = empty_authority("input-template");
        for run_input in [
            RegressionExecutionInput::Missing,
            RegressionExecutionInput::Resolved(invalid_input.clone()),
        ] {
            let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
                config: &MoaConfig::default(),
                tenant_id: TenantId::new(),
                skill_name: "input-template",
                skill_input_schema: &skill_input_schema,
                template: &template,
                run_input: &run_input,
                catalog: &catalog,
                authorization: &authorization,
                operation_key: &format!(
                    "skill_regression:{}:{}",
                    uuid::Uuid::now_v7(),
                    "c".repeat(64)
                ),
            })
            .expect("typed compiler rejection is still audited");

            assert!(!compiled.accepted);
            assert_eq!(
                compile_outcome_and_candidate_hash(&compiled).0,
                ExecutionCompileOutcome::NeedsInput
            );
        }
    }

    #[test]
    fn execution_template_compiles_with_real_governed_capability_authority() {
        // Pins: skill regression uses the same live governed catalog builder and exact
        // capability authorization as production execution planning.
        let registry = ToolRegistry::default_local();
        let response = build_capability_response(&registry.capability_registrations(), &[], &[])
            .expect("production capability catalog should build");
        let capability = response
            .catalog
            .capabilities
            .iter()
            .find(|capability| capability.reference.name == "file_read")
            .expect("default governed file_read capability exists");
        let template =
            governed_capability_template(&capability.reference, &capability.output_schema);
        let run_input = json!({});
        let run_input = RegressionExecutionInput::Resolved(run_input);
        let authorization = ExecutionAuthorizationEnvelope {
            capability_refs: response
                .catalog
                .capabilities
                .iter()
                .map(|entry| entry.reference.clone())
                .collect(),
            skill_refs: vec![moa_artifacts::reference::ArtifactRef::artifact(
                moa_artifacts::document::ArtifactKind::Skill,
                "governed-template",
            )],
        };

        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &MoaConfig::default(),
            tenant_id: TenantId::new(),
            skill_name: "governed-template",
            skill_input_schema: &json!({"type": "object"}),
            template: &template,
            run_input: &run_input,
            catalog: &response.catalog,
            authorization: &authorization,
            operation_key: &format!(
                "skill_regression:{}:{}",
                uuid::Uuid::now_v7(),
                "d".repeat(64)
            ),
        })
        .expect("authorized governed capability compiles");

        assert!(compiled.accepted);
        assert_eq!(
            compile_outcome_and_candidate_hash(&compiled).0,
            ExecutionCompileOutcome::Accepted
        );
    }

    #[test]
    fn execution_template_rejects_unauthorized_governed_capability_as_unsupported() {
        // Pins: catalog presence alone never authorizes a governed capability.
        let registry = ToolRegistry::default_local();
        let response = build_capability_response(&registry.capability_registrations(), &[], &[])
            .expect("production capability catalog should build");
        let capability = response
            .catalog
            .capabilities
            .iter()
            .find(|capability| capability.reference.name == "file_read")
            .expect("default governed file_read capability exists");
        let template =
            governed_capability_template(&capability.reference, &capability.output_schema);
        let run_input = json!({});
        let run_input = RegressionExecutionInput::Resolved(run_input);
        let authorization = ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: vec![moa_artifacts::reference::ArtifactRef::artifact(
                moa_artifacts::document::ArtifactKind::Skill,
                "governed-template",
            )],
        };
        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &MoaConfig::default(),
            tenant_id: TenantId::new(),
            skill_name: "governed-template",
            skill_input_schema: &json!({"type": "object"}),
            template: &template,
            run_input: &run_input,
            catalog: &response.catalog,
            authorization: &authorization,
            operation_key: &format!(
                "skill_regression:{}:{}",
                uuid::Uuid::now_v7(),
                "e".repeat(64)
            ),
        })
        .expect("unauthorized compiler outcome is still audited");

        assert!(!compiled.accepted);
        assert_eq!(
            compile_outcome_and_candidate_hash(&compiled).0,
            ExecutionCompileOutcome::Unsupported
        );
    }
}
