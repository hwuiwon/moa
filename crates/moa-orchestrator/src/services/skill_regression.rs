//! Review-boundary regression reporting for proposed skill updates.

use std::sync::Arc;
use std::{future::Future, pin::Pin};

use moa_artifacts::registry::ArtifactFile;
use moa_core::{ActionRuleScope, LearningCandidate, MoaConfig, Result};
use moa_core::{LLMProvider, MoaError, ModelTask};
use moa_eval::EvalEngine;
use moa_eval_core::engine::EvalRun;
use moa_eval_core::{
    ActionPolicyOverride, AgentConfig, EngineOptions, EvalResult, EvalScoreValue, EvalStatus,
    Evaluator, EvaluatorOptions, InstructionOverride, TestSuite, build_evaluators, evaluate_run,
};
use moa_providers::ProviderRegistry;
use moa_skills::artifact::skill_definition_from_package;
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::registry::SkillRegistry;
use moa_skills::regression::{SkillRegressionSummary, compare_scores};
use serde_json::{Value, json};

const DEFAULT_SKILL_TEST_BUDGET_DOLLARS: f64 = 0.50;
const DEFAULT_SKILL_EVALUATORS: &[&str] = &["trajectory", "output", "tool_success"];
/// Floor applied when a generated suite carries no (or a zero) case timeout.
const DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS: u64 = 90;

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
        }
    }

    fn blocked(report: Value, rejection_reason: String) -> Self {
        Self {
            report,
            allow_promotion: false,
            rejection_reason: Some(rejection_reason),
            execution: SkillRegressionExecution::Blocked,
            held_out_sources: 0,
        }
    }
}

/// Builds the review-time regression report for accepting a skill candidate.
pub async fn skill_acceptance_regression_report(
    config: MoaConfig,
    providers: Arc<ProviderRegistry>,
    registry: SkillRegistry,
    scope: ActionRuleScope,
    candidate: LearningCandidate,
    draft_files: Vec<ArtifactFile>,
) -> Result<SkillRegressionGate> {
    let Some(generated_suite) = generated_suite_payload(&candidate.payload) else {
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

    let mut suite = match toml::from_str::<TestSuite>(generated_suite.source_text) {
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
    // A missing suite timeout parses as zero, which times every case out
    // instantly and rejects the candidate for a fixture defect rather than a
    // behavior regression. Floor it instead of trusting the TOML default.
    if suite.default_timeout_seconds == 0 {
        suite.default_timeout_seconds = DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS;
    }

    let candidate_package = SkillPackage::new(
        draft_files
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

    // Smoke-run a candidate procedure before spending any eval budget:
    // document validation catches structural document defects, while this
    // catches graphs that error immediately at runtime.
    let definition = skill_definition_from_package(&candidate_package)?;
    if let Some(smoke_error) = definition
        .procedure
        .as_ref()
        .and_then(procedure_smoke_error)
    {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "candidate procedure failed its smoke run",
                "error": smoke_error,
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "candidate procedure failed its smoke run".to_string(),
        ));
    }

    // An unavailable provider is an operational failure, not a property of the
    // candidate: surface an error so the review can be retried after the
    // deployment is fixed, instead of silently waiving the gate.
    let provider =
        providers.provider_for_model(Some(config.model_for_task(ModelTask::MainLoop)))?;

    // Held-out material the candidate was not derived from: the previous
    // promoted revision's own suite (it rode that revision's package) plus
    // sibling suites accumulated from deduped recurring sessions.
    let held_out = collect_held_out_pool(previous_package.as_ref(), &candidate.payload);

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
        ));
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
        ))
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
    candidate_payload: &Value,
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

    let sibling_entries = candidate_payload
        .get("accumulated_regression_suites")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (index, entry) in sibling_entries.iter().enumerate() {
        let Some(source_text) = entry.get("source_text").and_then(Value::as_str) else {
            skipped.push(format!("sibling suite {index} missing source_text"));
            continue;
        };
        match toml::from_str::<TestSuite>(source_text) {
            Ok(suite) => {
                source_count += 1;
                cases.extend(prefixed_cases(&format!("sib{index}"), suite));
            }
            Err(error) => skipped.push(format!("sibling suite {index} unreadable: {error}")),
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

struct GeneratedSuitePayload<'a> {
    relative_path: Option<&'a str>,
    source_format: Option<&'a str>,
    source_text: &'a str,
}

impl GeneratedSuitePayload<'_> {
    fn summary(&self) -> Value {
        json!({
            "relative_path": self.relative_path,
            "source_format": self.source_format,
            "source_text_present": true,
        })
    }

    fn summary_with_suite(&self, suite: &TestSuite) -> Value {
        json!({
            "relative_path": self.relative_path,
            "source_format": self.source_format,
            "source_text_present": true,
            "suite_name": suite.name,
            "case_count": suite.cases.len(),
        })
    }
}

fn generated_suite_payload(payload: &Value) -> Option<GeneratedSuitePayload<'_>> {
    let suite = payload.get("generated_regression_suite")?;
    Some(GeneratedSuitePayload {
        relative_path: suite.get("relative_path").and_then(Value::as_str),
        source_format: suite.get("source_format").and_then(Value::as_str),
        source_text: suite.get("source_text").and_then(Value::as_str)?,
    })
}

fn skill_name(candidate: &LearningCandidate) -> Option<String> {
    candidate
        .payload
        .get("artifact_name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| candidate.target_label.clone())
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

/// Smoke-runs a procedure graph one step and returns the failure, if any.
///
/// The pure interpreter advances from the start node with empty input. Reaching
/// completion, a blocked side-effect node, or ready requests all prove the
/// graph starts executing. A condition that cannot match on empty input is
/// input-dependent rather than structural, so it passes; every other
/// interpreter error means the graph would fail on its first real run and the
/// candidate must not promote.
fn procedure_smoke_error(
    procedure: &moa_artifacts::procedure::ProcedureDefinition,
) -> Option<String> {
    use moa_skills::procedure::error::ProcedureError;
    use moa_skills::procedure::interpreter::{ProcedureExecutionState, ProcedureInterpreter};

    let smoke = ProcedureInterpreter::new(procedure).advance(ProcedureExecutionState::new(
        uuid::Uuid::now_v7(),
        json!({}),
    ));
    match smoke {
        Ok(_) => None,
        Err(ProcedureError::NoMatchingOutgoingEdge { .. }) => None,
        Err(error) => Some(error.to_string()),
    }
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

fn map_eval_error(error: moa_eval_core::EvalError) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use moa_artifacts::procedure::ProcedureDefinition;
    use serde_json::json;

    use super::{collect_held_out_pool, procedure_smoke_error};

    #[test]
    fn held_out_pool_merges_sibling_suites_with_prefixed_case_names() {
        // Pins: accumulated sibling suites merge into one pool suite with source-prefixed
        // case names, and unreadable entries are skipped with a recorded reason instead
        // of rejecting the candidate.
        let payload = json!({
            "accumulated_regression_suites": [
                {
                    "source_experience_id": "a",
                    "source_text": "[suite]\nname = \"s0\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"smoke\"\ninput = \"run\"\n",
                },
                {
                    "source_experience_id": "b",
                    "source_text": "this is [not toml",
                },
            ],
        });

        let pool = collect_held_out_pool(None, &payload);

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
    fn held_out_pool_is_empty_without_material() {
        // Pins: a first revision of a novel task has no held-out material and the report
        // base says so instead of implying a split ran.
        let pool = collect_held_out_pool(None, &json!({}));

        assert_eq!(pool.source_count, 0);
        assert!(pool.suite.is_none());
        assert_eq!(pool.report_base()["decision"], "no_material");
    }

    fn procedure(value: serde_json::Value) -> ProcedureDefinition {
        serde_json::from_value(value).expect("procedure definition parses")
    }

    #[test]
    fn smoke_passes_a_start_to_end_graph() {
        // Pins: a graph that reaches its end node smokes clean.
        let procedure = procedure(json!({
            "nodes": [
                {"id": "start", "kind": "start"},
                {"id": "done", "kind": "end"},
            ],
            "edges": [{"from": "start", "to": "done"}],
        }));

        assert_eq!(procedure_smoke_error(&procedure), None);
    }

    #[test]
    fn smoke_rejects_an_edge_to_a_missing_node() {
        // Pins: a structurally broken graph (dangling edge) blocks promotion with the
        // interpreter error preserved for the reviewer.
        let procedure = procedure(json!({
            "nodes": [{"id": "start", "kind": "start"}],
            "edges": [{"from": "start", "to": "ghost"}],
        }));

        let error = procedure_smoke_error(&procedure).expect("dangling edge must fail smoke");
        assert!(
            error.contains("ghost"),
            "error names the missing node: {error}"
        );
    }

    #[test]
    fn smoke_treats_unmatched_input_conditions_as_input_dependent() {
        // Pins: a condition that cannot match on empty smoke input is not a structural
        // defect — real runs supply inputs, so the candidate still promotes.
        let procedure = procedure(json!({
            "nodes": [
                {"id": "start", "kind": "start"},
                {
                    "id": "gate",
                    "kind": "condition",
                    "condition": {"type": "exists", "path": "input.ticket"},
                },
                {"id": "done", "kind": "end"},
            ],
            "edges": [
                {"from": "start", "to": "gate"},
                {"from": "gate", "to": "done"},
            ],
        }));

        assert_eq!(procedure_smoke_error(&procedure), None);
    }
}
