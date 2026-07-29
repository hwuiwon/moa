//! Built-in evaluators and scoring helpers for MOA eval runs.

mod output_match;
mod threshold;
mod tool_success;
mod trajectory_match;

use crate::engine::{EvalRun, RunSummary};
use crate::{Error, EvalScore, EvalStatus, Evaluator, Result, TestSuite};

pub use output_match::OutputMatchEvaluator;
pub use threshold::ThresholdEvaluator;
pub use tool_success::ToolSuccessEvaluator;
pub use trajectory_match::TrajectoryMatchEvaluator;

/// Post-hoc threshold configuration passed into the built-in evaluator factory.
///
/// These bounds only score an already-completed result; admission limits and
/// reservations are what actually stop work from being dispatched.
#[derive(Debug, Clone, Default)]
pub struct EvaluatorOptions {
    /// Dollar cost above which a result is scored as failing.
    pub max_cost_dollars: Option<f64>,
    /// Latency per result, in milliseconds, above which it is scored as failing.
    pub max_latency_ms: Option<u64>,
    /// Total tokens above which a result is scored as failing.
    pub max_tokens: Option<usize>,
    /// Tool calls above which a result is scored as failing.
    pub max_tool_calls: Option<usize>,
    /// Turns above which a result is scored as failing.
    pub max_turns: Option<usize>,
}

/// Builds the requested evaluator set by name.
pub fn build_evaluators(
    names: &[String],
    options: &EvaluatorOptions,
) -> Result<Vec<Box<dyn Evaluator>>> {
    let mut evaluators: Vec<Box<dyn Evaluator>> = Vec::new();
    for name in names {
        match name.as_str() {
            "trajectory" | "trajectory_match" => {
                evaluators.push(Box::new(TrajectoryMatchEvaluator));
            }
            "output" | "output_match" => {
                evaluators.push(Box::new(OutputMatchEvaluator));
            }
            "threshold" => {
                evaluators.push(Box::new(ThresholdEvaluator {
                    max_cost_dollars: options.max_cost_dollars,
                    max_latency_ms: options.max_latency_ms,
                    max_tokens: options.max_tokens,
                    max_tool_calls: options.max_tool_calls,
                    max_turns: options.max_turns,
                }));
            }
            "tool_success" => {
                evaluators.push(Box::new(ToolSuccessEvaluator));
            }
            other => {
                return Err(Error::InvalidConfig(format!("unknown evaluator '{other}'")));
            }
        }
    }
    Ok(evaluators)
}

/// Applies evaluator scores to every result in a completed run.
pub async fn evaluate_run(
    suite: &TestSuite,
    run: &mut EvalRun,
    evaluators: &[Box<dyn Evaluator>],
) -> Result<()> {
    for result in &mut run.results {
        let Some(case) = suite
            .cases
            .iter()
            .find(|case| case.name == result.test_case)
        else {
            return Err(Error::InvalidConfig(format!(
                "result references unknown test case '{}'",
                result.test_case
            )));
        };

        for evaluator in evaluators {
            let scores = evaluator.evaluate(case, result).await?;
            if result.status == EvalStatus::Passed && scores.iter().any(score_is_failure) {
                result.status = EvalStatus::Failed;
            }
            result.scores.extend(scores);
        }
    }

    run.summary = RunSummary::from_results(&run.results);
    Ok(())
}

/// Returns whether a score should downgrade a successful run to `Failed`.
pub fn score_is_failure(score: &EvalScore) -> bool {
    match &score.value {
        crate::EvalScoreValue::Numeric(value) => *value < 0.5,
        crate::EvalScoreValue::Boolean(value) => !value,
        crate::EvalScoreValue::Categorical(_) => false,
    }
}
