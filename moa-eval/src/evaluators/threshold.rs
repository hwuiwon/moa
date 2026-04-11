//! Threshold-based evaluator for cost, latency, token, turn, and tool-call limits.

use crate::{EvalResult, EvalScore, Evaluator, Result, ScoreValue, TestCase};

/// Enforces resource thresholds on a completed eval result.
#[derive(Debug, Clone, Default)]
pub struct ThresholdEvaluator {
    /// Maximum allowed dollar cost per result.
    pub max_cost_dollars: Option<f64>,
    /// Maximum allowed latency in milliseconds.
    pub max_latency_ms: Option<u64>,
    /// Maximum allowed total tokens.
    pub max_tokens: Option<usize>,
    /// Maximum allowed tool calls.
    pub max_tool_calls: Option<usize>,
    /// Maximum allowed turns.
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
        value: ScoreValue::Boolean(passed),
        comment: Some(comment),
    }
}

#[cfg(test)]
mod tests {
    use super::ThresholdEvaluator;
    use crate::{EvalMetrics, EvalResult, Evaluator, ScoreValue, TestCase};

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
        assert_eq!(scores[0].value, ScoreValue::Boolean(false));
    }
}
