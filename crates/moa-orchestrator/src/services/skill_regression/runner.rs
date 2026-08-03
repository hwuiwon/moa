//! Eval-engine execution and score aggregation for skill regression suites.

use std::{future::Future, pin::Pin, sync::Arc};

use moa_config::MoaConfig;
use moa_core::{
    error::{MoaError, Result},
    traits::LLMProvider,
};
use moa_eval::EvalEngine;
use moa_eval_core::engine::EvalRun;
use moa_eval_core::{
    ActionPolicyOverride, AgentConfig, EngineOptions, EvalResult, EvalScoreValue, EvalStatus,
    Evaluator, EvaluatorOptions, InstructionOverride, TestSuite, build_evaluators, evaluate_run,
};
use moa_skills::regression::SkillRegressionSummary;

use super::DEFAULT_SKILL_EVALUATORS;

type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Paired eval runs for the serving and candidate skill revisions.
pub(super) struct ExecutedRegressionRuns {
    /// Result from the previously serving revision.
    pub(super) previous: EvalRun,
    /// Result from the candidate revision.
    pub(super) candidate: EvalRun,
}

/// Executes a suite against the candidate revision when there is no serving baseline.
pub(super) async fn execute_candidate_only(
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

/// Executes the same suite against the serving and candidate revisions in order.
pub(super) async fn execute_previous_and_candidate(
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

/// Reduces one eval run to the aggregate metrics used by the regression decision.
pub(super) fn summarize_regression_run(run: &EvalRun) -> SkillRegressionSummary {
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

/// Computes the acceptance score from blocking evaluator rows only.
pub(super) fn result_score(result: &EvalResult) -> f64 {
    let mut total = 0.0;
    let mut count = 0usize;
    for score in result
        .scores
        .iter()
        .filter(|score| score.gate.is_blocking())
    {
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
        match result.status {
            EvalStatus::Passed | EvalStatus::Skipped => 1.0,
            EvalStatus::Failed | EvalStatus::Error | EvalStatus::Timeout => 0.0,
        }
    } else {
        total / count as f64
    }
}

/// Returns whether any case failed because execution errored or timed out.
pub(super) fn run_has_execution_failure(run: &EvalRun) -> bool {
    run.results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Error | EvalStatus::Timeout))
}

/// Estimates one suite run's provider cost before starting review execution.
pub(super) fn estimate_suite_cost(suite: &TestSuite, llm: &dyn LLMProvider) -> f64 {
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

fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

fn map_eval_error(error: moa_eval_core::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
