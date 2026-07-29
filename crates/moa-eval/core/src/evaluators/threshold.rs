//! Post-hoc threshold assertions on cost, latency, token, turn, and tool-call
//! counts observed by a completed eval run.
//!
//! These are score assertions, not admission control. The work has already been
//! dispatched and paid for by the time an evaluator runs; hard limits are
//! enforced before dispatch by
//! [`crate::admission::EvalAdmissionPolicy`] and the reservation ledger in
//! [`moa_core::types::resource`].

use crate::{EvalResult, EvalScore, EvalScoreValue, Evaluator, Result, TestCase};

/// Scores a completed eval result against expected resource thresholds.
///
/// Every field is an assertion boundary used to emit a pass/fail score. None of
/// them can prevent work from happening.
#[derive(Debug, Clone, Default)]
pub struct ThresholdEvaluator {
    /// Dollar cost above which the result is scored as failing.
    pub max_cost_dollars: Option<f64>,
    /// Latency in milliseconds above which the result is scored as failing.
    pub max_latency_ms: Option<u64>,
    /// Total tokens above which the result is scored as failing.
    pub max_tokens: Option<usize>,
    /// Tool calls above which the result is scored as failing.
    pub max_tool_calls: Option<usize>,
    /// Turns above which the result is scored as failing.
    pub max_turns: Option<usize>,
}

#[async_trait::async_trait]
impl Evaluator for ThresholdEvaluator {
    fn name(&self) -> &str {
        "threshold"
    }

    async fn evaluate(&self, _case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let mut scores = Vec::new();
        if let Some(max_cost) = self.max_cost_dollars {
            scores.push(limit_score(
                self.name(),
                "cost_within_budget",
                result.metrics.cost_dollars <= max_cost,
                format!("${:.4} / ${:.4} max", result.metrics.cost_dollars, max_cost),
            ));
        }
        if let Some(max_latency) = self.max_latency_ms {
            scores.push(limit_score(
                self.name(),
                "latency_within_threshold",
                result.metrics.latency_ms <= max_latency,
                format!("{}ms / {}ms max", result.metrics.latency_ms, max_latency),
            ));
        }
        if let Some(max_tokens) = self.max_tokens {
            scores.push(limit_score(
                self.name(),
                "tokens_within_threshold",
                result.metrics.total_tokens <= max_tokens,
                format!("{} / {} max", result.metrics.total_tokens, max_tokens),
            ));
        }
        if let Some(max_tool_calls) = self.max_tool_calls {
            scores.push(limit_score(
                self.name(),
                "tool_calls_within_threshold",
                result.metrics.tool_call_count <= max_tool_calls,
                format!(
                    "{} / {} max",
                    result.metrics.tool_call_count, max_tool_calls
                ),
            ));
        }
        if let Some(max_turns) = self.max_turns {
            scores.push(limit_score(
                self.name(),
                "turns_within_threshold",
                result.metrics.turn_count <= max_turns,
                format!("{} / {} max", result.metrics.turn_count, max_turns),
            ));
        }
        Ok(scores)
    }
}

fn limit_score(evaluator: &str, name: &str, passed: bool, comment: String) -> EvalScore {
    EvalScore {
        evaluator: evaluator.to_string(),
        name: name.to_string(),
        value: EvalScoreValue::Boolean(passed),
        comment: Some(comment),
    }
}

#[cfg(test)]
mod tests {
    use super::ThresholdEvaluator;
    use crate::{EvalMetrics, EvalResult, EvalScoreValue, Evaluator, TestCase};

    #[tokio::test]
    async fn cost_over_budget_fails_boolean_score() {
        let evaluator = ThresholdEvaluator {
            max_cost_dollars: Some(0.01),
            ..ThresholdEvaluator::default()
        };
        let result = EvalResult {
            metrics: EvalMetrics {
                cost_dollars: 0.05,
                ..EvalMetrics::default()
            },
            ..EvalResult::default()
        };

        let scores = evaluator
            .evaluate(&TestCase::default(), &result)
            .await
            .expect("score");
        assert_eq!(scores[0].value, EvalScoreValue::Boolean(false));
    }
}
