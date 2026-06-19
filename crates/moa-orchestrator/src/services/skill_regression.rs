//! Review-boundary regression reporting for proposed skill updates.

#[cfg(feature = "internal-eval-runner")]
use std::io::ErrorKind;
#[cfg(feature = "internal-eval-runner")]
use std::path::{Path, PathBuf};
#[cfg(feature = "internal-eval-runner")]
use std::sync::Arc;

use moa_artifacts::registry::ArtifactFile;
#[cfg(feature = "internal-eval-runner")]
use moa_core::{LLMProvider, MoaError, ModelTask};
use moa_core::{LearningCandidate, MemoryScope, MoaConfig, Result};
#[cfg(feature = "internal-eval-runner")]
use moa_eval::{
    ActionPolicyOverride, AgentConfig, EngineOptions, EvalEngine, EvalResult, EvalRun, EvalStatus,
    Evaluator, EvaluatorOptions, ScoreValue, SkillOverride, TestSuite, build_evaluators,
    evaluate_run,
};
#[cfg(feature = "internal-eval-runner")]
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::registry::SkillRegistry;
#[cfg(feature = "internal-eval-runner")]
use moa_skills::{
    format::slugify_skill_name,
    regression::{SkillRegressionSummary, compare_scores},
};
use serde_json::{Value, json};
#[cfg(feature = "internal-eval-runner")]
use tokio::fs;
#[cfg(feature = "internal-eval-runner")]
use uuid::Uuid;

#[cfg(feature = "internal-eval-runner")]
use crate::services::llm_gateway::ProviderRegistry;

#[cfg(feature = "internal-eval-runner")]
const DEFAULT_SKILL_TEST_BUDGET_DOLLARS: f64 = 0.50;
#[cfg(feature = "internal-eval-runner")]
const DEFAULT_SKILL_EVALUATORS: &[&str] = &["trajectory", "output", "tool_success"];

/// Outcome of review-time regression evaluation for a skill proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRegressionGate {
    /// Structured report to attach to the candidate evaluation payload.
    pub report: Value,
    /// Whether review acceptance may continue to publish and materialize the skill.
    pub allow_promotion: bool,
    /// Human-readable rejection reason when regression blocks promotion.
    pub rejection_reason: Option<String>,
}

impl SkillRegressionGate {
    fn non_blocking(report: Value) -> Self {
        Self {
            report,
            allow_promotion: true,
            rejection_reason: None,
        }
    }
}

/// Builds the review-time regression report for accepting a skill candidate.
pub async fn skill_acceptance_regression_report(
    config: MoaConfig,
    #[cfg(feature = "internal-eval-runner")] providers: Arc<ProviderRegistry>,
    registry: SkillRegistry,
    scope: MemoryScope,
    candidate: LearningCandidate,
    draft_files: Vec<ArtifactFile>,
) -> Result<SkillRegressionGate> {
    #[cfg(feature = "internal-eval-runner")]
    {
        internal_eval_regression_report(config, providers, registry, scope, candidate, draft_files)
            .await
    }

    #[cfg(not(feature = "internal-eval-runner"))]
    {
        let _ = (config, registry, scope, draft_files);
        Ok(SkillRegressionGate::non_blocking(json!({
            "regression_execution": "unavailable",
            "runner": "moa-eval",
            "reason": "internal-eval-runner feature disabled",
            "generated_suite": generated_suite_summary(&candidate.payload),
            "previous_skill": null,
        })))
    }
}

#[cfg(feature = "internal-eval-runner")]
async fn internal_eval_regression_report(
    config: MoaConfig,
    providers: Arc<ProviderRegistry>,
    registry: SkillRegistry,
    scope: MemoryScope,
    candidate: LearningCandidate,
    draft_files: Vec<ArtifactFile>,
) -> Result<SkillRegressionGate> {
    internal_eval_regression_report_inner(
        config,
        providers,
        registry,
        scope,
        candidate,
        draft_files,
    )
    .await
}

#[cfg(feature = "internal-eval-runner")]
async fn internal_eval_regression_report_inner(
    config: MoaConfig,
    providers: Arc<ProviderRegistry>,
    registry: SkillRegistry,
    scope: MemoryScope,
    candidate: LearningCandidate,
    draft_files: Vec<ArtifactFile>,
) -> Result<SkillRegressionGate> {
    let Some(generated_suite) = generated_suite_payload(&candidate.payload) else {
        return Ok(SkillRegressionGate::non_blocking(json!({
            "regression_execution": "unavailable",
            "runner": "moa-eval",
            "reason": "candidate has no generated regression suite",
            "generated_suite": null,
            "previous_skill": null,
        })));
    };

    let Some(skill_name) = skill_name(&candidate) else {
        return Ok(SkillRegressionGate::non_blocking(json!({
            "regression_execution": "unavailable",
            "runner": "moa-eval",
            "reason": "candidate payload missing skill name",
            "generated_suite": generated_suite.summary(),
            "previous_skill": null,
        })));
    };

    let previous_package = registry.load_package_by_name(&scope, &skill_name).await?;
    let Some(previous_package) = previous_package else {
        return Ok(SkillRegressionGate::non_blocking(json!({
            "regression_execution": "skipped",
            "runner": "moa-eval",
            "reason": "no previous active skill exists for comparison",
            "generated_suite": generated_suite.summary(),
            "previous_skill": null,
        })));
    };

    let suite = match toml::from_str::<TestSuite>(generated_suite.source_text) {
        Ok(suite) => suite,
        Err(error) => {
            return Ok(SkillRegressionGate::non_blocking(json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "generated regression suite could not be parsed",
                "error": error.to_string(),
                "generated_suite": generated_suite.summary(),
                "previous_skill": previous_skill_payload(&previous_package.skill),
            })));
        }
    };

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
    let previous_markdown = previous_package.skill_markdown()?.to_string();
    let candidate_markdown = candidate_package.skill_md.clone();
    let provider =
        match providers.provider_for_model(Some(config.model_for_task(ModelTask::MainLoop))) {
            Ok(provider) => provider,
            Err(error) => {
                return Ok(SkillRegressionGate::non_blocking(json!({
                    "regression_execution": "unavailable",
                    "runner": "moa-eval",
                    "reason": "review regression provider is unavailable",
                    "error": error.to_string(),
                    "generated_suite": generated_suite.summary(),
                    "previous_skill": previous_skill_payload(&previous_package.skill),
                })));
            }
        };

    let estimated_cost = estimate_suite_cost(&suite, provider.as_ref()) * 2.0;
    if estimated_cost > DEFAULT_SKILL_TEST_BUDGET_DOLLARS {
        return Ok(SkillRegressionGate::non_blocking(json!({
            "regression_execution": "skipped",
            "runner": "moa-eval",
            "reason": "estimated regression cost exceeds budget",
            "estimated_cost_dollars": estimated_cost,
            "budget_dollars": DEFAULT_SKILL_TEST_BUDGET_DOLLARS,
            "generated_suite": generated_suite.summary_with_suite(&suite),
            "previous_skill": previous_skill_payload(&previous_package.skill),
        })));
    }

    let executed = execute_previous_and_candidate(
        &config,
        &suite,
        &skill_name,
        &previous_markdown,
        &candidate_markdown,
        provider,
    )
    .await?;

    let previous_summary = summarize_regression_run(&executed.previous);
    let candidate_summary = summarize_regression_run(&executed.candidate);
    let has_execution_failure = run_has_execution_failure(&executed.previous)
        || run_has_execution_failure(&executed.candidate);
    let accepted = !has_execution_failure && compare_scores(&previous_summary, &candidate_summary);
    let report = json!({
        "regression_execution": "completed",
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
        "candidate": regression_summary_to_json(&candidate_summary),
    });

    if accepted {
        Ok(SkillRegressionGate::non_blocking(report))
    } else {
        Ok(SkillRegressionGate {
            report,
            allow_promotion: false,
            rejection_reason: Some(if has_execution_failure {
                "skill regression eval failed".to_string()
            } else {
                "skill regression rejected the proposed draft".to_string()
            }),
        })
    }
}

#[cfg(feature = "internal-eval-runner")]
struct GeneratedSuitePayload<'a> {
    relative_path: Option<&'a str>,
    source_format: Option<&'a str>,
    source_text: &'a str,
}

#[cfg(feature = "internal-eval-runner")]
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

#[cfg(feature = "internal-eval-runner")]
fn generated_suite_payload(payload: &Value) -> Option<GeneratedSuitePayload<'_>> {
    let suite = payload.get("generated_regression_suite")?;
    Some(GeneratedSuitePayload {
        relative_path: suite.get("relative_path").and_then(Value::as_str),
        source_format: suite.get("source_format").and_then(Value::as_str),
        source_text: suite.get("source_text").and_then(Value::as_str)?,
    })
}

#[cfg(not(feature = "internal-eval-runner"))]
fn generated_suite_summary(payload: &Value) -> Value {
    let Some(suite) = payload.get("generated_regression_suite") else {
        return Value::Null;
    };
    json!({
        "relative_path": suite.get("relative_path").and_then(Value::as_str),
        "source_format": suite.get("source_format").and_then(Value::as_str),
        "source_text_present": suite.get("source_text").and_then(Value::as_str).is_some(),
    })
}

#[cfg(feature = "internal-eval-runner")]
fn skill_name(candidate: &LearningCandidate) -> Option<String> {
    candidate
        .payload
        .get("artifact_name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| candidate.target_label.clone())
}

#[cfg(feature = "internal-eval-runner")]
struct ExecutedRegressionRuns {
    previous: EvalRun,
    candidate: EvalRun,
}

#[cfg(feature = "internal-eval-runner")]
async fn execute_previous_and_candidate(
    config: &MoaConfig,
    suite: &TestSuite,
    skill_name: &str,
    previous_markdown: &str,
    candidate_markdown: &str,
    provider: Arc<dyn LLMProvider>,
) -> Result<ExecutedRegressionRuns> {
    let temp_root = std::env::temp_dir().join(format!("moa-skill-review-{}", Uuid::now_v7()));
    let previous_dir =
        materialize_skill_dir(&temp_root.join("previous"), skill_name, previous_markdown).await?;
    let candidate_dir =
        materialize_skill_dir(&temp_root.join("candidate"), skill_name, candidate_markdown).await?;

    let previous = execute_skill_suite(
        config,
        suite,
        &previous_dir,
        skill_name,
        provider.clone(),
        "previous",
    )
    .await;
    let candidate = match previous {
        Ok(previous) => {
            let candidate = execute_skill_suite(
                config,
                suite,
                &candidate_dir,
                skill_name,
                provider,
                "candidate",
            )
            .await;
            candidate.map(|candidate| ExecutedRegressionRuns {
                previous,
                candidate,
            })
        }
        Err(error) => Err(error),
    };

    let _ = remove_dir_if_exists(&temp_root).await;
    candidate
}

#[cfg(feature = "internal-eval-runner")]
async fn execute_skill_suite(
    config: &MoaConfig,
    suite: &TestSuite,
    skill_dir: &Path,
    skill_name: &str,
    provider: Arc<dyn LLMProvider>,
    label: &str,
) -> Result<EvalRun> {
    let agent_config = skill_agent_config(skill_name, skill_dir, label);
    let engine = EvalEngine::new(
        config.clone(),
        EngineOptions {
            parallel: 1,
            temp_dir: std::env::temp_dir().join("moa-eval-skill-review"),
            ..EngineOptions::default()
        },
    )
    .map_err(map_eval_error)?;
    let mut run = engine
        .run_suite_with_provider(suite, std::slice::from_ref(&agent_config), provider)
        .await
        .map_err(map_eval_error)?;
    let evaluators = default_skill_evaluators()?;
    evaluate_run(suite, &mut run, &evaluators)
        .await
        .map_err(map_eval_error)?;
    Ok(run)
}

#[cfg(feature = "internal-eval-runner")]
fn skill_agent_config(skill_name: &str, skill_dir: &Path, label: &str) -> AgentConfig {
    AgentConfig {
        name: format!("skill-{skill_name}-{label}"),
        skills: SkillOverride {
            include: vec![skill_dir.to_string_lossy().into_owned()],
            exclude: Vec::new(),
            exclusive: true,
        },
        permissions: ActionPolicyOverride::default(),
        ..AgentConfig::default()
    }
}

#[cfg(feature = "internal-eval-runner")]
async fn materialize_skill_dir(root: &Path, skill_name: &str, markdown: &str) -> Result<PathBuf> {
    let slug = slugify_skill_name(skill_name);
    let skill_dir = root.join(slug);
    fs::create_dir_all(&skill_dir).await?;
    fs::write(skill_dir.join("SKILL.md"), markdown).await?;
    Ok(skill_dir)
}

#[cfg(feature = "internal-eval-runner")]
async fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(MoaError::StorageError(error.to_string())),
    }
}

#[cfg(feature = "internal-eval-runner")]
fn default_skill_evaluators() -> Result<Vec<Box<dyn Evaluator>>> {
    let names = DEFAULT_SKILL_EVALUATORS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    build_evaluators(&names, &EvaluatorOptions::default()).map_err(map_eval_error)
}

#[cfg(feature = "internal-eval-runner")]
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

#[cfg(feature = "internal-eval-runner")]
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
            ScoreValue::Numeric(value) => {
                total += *value;
                count += 1;
            }
            ScoreValue::Boolean(value) => {
                total += if *value { 1.0 } else { 0.0 };
                count += 1;
            }
            ScoreValue::Categorical(_) => {}
        }
    }

    if count == 0 {
        1.0
    } else {
        total / count as f64
    }
}

#[cfg(feature = "internal-eval-runner")]
fn run_has_execution_failure(run: &EvalRun) -> bool {
    run.results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Error | EvalStatus::Timeout))
}

#[cfg(feature = "internal-eval-runner")]
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

#[cfg(feature = "internal-eval-runner")]
fn previous_skill_payload(skill: &moa_skills::registry::Skill) -> Value {
    json!({
        "skill_uid": skill.skill_uid,
        "version": skill.version,
        "name": skill.name,
    })
}

#[cfg(feature = "internal-eval-runner")]
fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

#[cfg(feature = "internal-eval-runner")]
fn regression_summary_to_json(summary: &SkillRegressionSummary) -> Value {
    json!({
        "average_score": summary.average_score,
        "failed_runs": summary.failed_runs,
        "total_runs": summary.total_runs,
        "total_cost_dollars": summary.total_cost_dollars,
    })
}

#[cfg(feature = "internal-eval-runner")]
fn map_eval_error(error: moa_eval_core::EvalError) -> MoaError {
    MoaError::StorageError(error.to_string())
}
