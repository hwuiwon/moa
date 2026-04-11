//! Evaluator that scores successful tool execution rate.

use crate::{EvalResult, EvalScore, Evaluator, Result, ScoreValue, TestCase};

/// Scores the ratio of successful tool calls for a run.
pub struct ToolSuccessEvaluator;

#[async_trait::async_trait]
impl Evaluator for ToolSuccessEvaluator {
    fn name(&self) -> &str {
        "tool_success"
    }

    async fn evaluate(&self, _case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        if result.trajectory.is_empty() {
            return Ok(Vec::new());
        }

        let success_count = result.trajectory.iter().filter(|step| step.success).count();
        let total = result.trajectory.len();
        let rate = success_count as f64 / total as f64;

        Ok(vec![EvalScore {
            evaluator: self.name().to_string(),
            name: "tool_success_rate".to_string(),
            value: ScoreValue::Numeric(rate),
            comment: Some(format!("{success_count}/{total} succeeded")),
        }])
    }
}
