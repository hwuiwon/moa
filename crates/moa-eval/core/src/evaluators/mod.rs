//! Built-in evaluators and scoring helpers for MOA eval runs.
//!
//! Two layers live here, and the split is load-bearing:
//!
//! - **Assertion evaluators** ([`crate::assertion::AssertionEvaluator`]) settle
//!   what a case *claimed*, by reading captured evidence. They are the gate.
//! - **Run evaluators** ([`crate::evaluator::Evaluator`]) score properties of the
//!   run itself — cost, latency, tool-success rate, path similarity. Only the
//!   resource thresholds still gate; the text and path scorers are now explicit
//!   diagnostics, because a fractional similarity number is not evidence.

pub mod action_assertions;
pub mod communication;
pub mod environment_state;
mod output_match;
pub mod semantic_history;
mod threshold;
mod tool_success;
mod trajectory_match;

use std::sync::Arc;

use crate::assertion::{AssertionEvaluator, AssertionOutcome, GateEffect, evaluate_assertions};
use crate::engine::{EvalRun, RunSummary};
use crate::{Error, EvalScore, EvalStatus, Evaluator, Result, TestSuite};

pub use action_assertions::{
    APPROVAL_BEFORE_ACTION_EVALUATOR_ID, ApprovalBeforeActionEvaluator, ApprovalPair,
    ORDERED_ACTIONS_EVALUATOR_ID, OrderedActionsEvaluator, PROHIBITED_ACTIONS_EVALUATOR_ID,
    ProhibitedActionsEvaluator, REQUIRED_ACTIONS_EVALUATOR_ID, RequiredAction,
    RequiredActionsEvaluator,
};
pub use communication::{TEXT_MATCH_EVALUATOR_ID, TextMatchEvaluator};
pub use environment_state::{ENVIRONMENT_STATE_EVALUATOR_ID, EnvironmentStateEvaluator};
pub use output_match::OutputMatchEvaluator;
pub use semantic_history::{HISTORY_RECALL_EVALUATOR_ID, HistoryRecallEvaluator, HistoryScope};
pub use threshold::ThresholdEvaluator;
pub use tool_success::ToolSuccessEvaluator;
pub use trajectory_match::TrajectoryMatchEvaluator;

/// Returns every built-in assertion evaluator, one per registered id.
///
/// The registry is built from this list, so adding an evaluator here is the
/// only way to make a new assertion behavior selectable by an authored case.
#[must_use]
pub fn builtin_assertion_evaluators() -> Vec<Arc<dyn AssertionEvaluator>> {
    vec![
        Arc::new(EnvironmentStateEvaluator),
        Arc::new(TextMatchEvaluator),
        Arc::new(HistoryRecallEvaluator),
        Arc::new(RequiredActionsEvaluator),
        Arc::new(ProhibitedActionsEvaluator),
        Arc::new(OrderedActionsEvaluator),
        Arc::new(ApprovalBeforeActionEvaluator),
    ]
}

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

/// Builds the requested run-evaluator set by name.
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

/// Evaluates a completed run: typed assertions first, then run evaluators.
///
/// Assertions always run, whether or not the caller asked for any named
/// evaluator, so a suite cannot dodge its own claims by configuring an empty
/// evaluator list. A blocking assertion failure downgrades the result even when
/// every run evaluator is happy.
pub async fn evaluate_run(
    suite: &TestSuite,
    run: &mut EvalRun,
    evaluators: &[Box<dyn Evaluator>],
) -> Result<()> {
    let registry = crate::assertion::builtin_registry();
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

        let outcomes = evaluate_assertions(registry, case, result.evidence.as_ref());
        if result.status == EvalStatus::Passed
            && outcomes.iter().any(AssertionOutcome::is_gate_failure)
        {
            result.status = EvalStatus::Failed;
        }
        result
            .scores
            .extend(outcomes.iter().map(assertion_outcome_score));
        result.assertions.extend(outcomes);

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

/// Renders one assertion outcome as a reportable score.
fn assertion_outcome_score(outcome: &AssertionOutcome) -> EvalScore {
    EvalScore {
        evaluator: outcome.evaluator.id.clone(),
        name: outcome.assertion_id.clone(),
        value: crate::EvalScoreValue::Boolean(outcome.passed),
        gate: outcome.gate_effect,
        comment: if outcome.passed {
            None
        } else {
            Some(outcome.diagnostic.clone())
        },
    }
}

/// Returns whether a score should downgrade a successful run to `Failed`.
///
/// A diagnostic score never does, no matter how low it is.
pub fn score_is_failure(score: &EvalScore) -> bool {
    if score.gate == GateEffect::Diagnostic {
        return false;
    }
    match &score.value {
        crate::EvalScoreValue::Numeric(value) => *value < 0.5,
        crate::EvalScoreValue::Boolean(value) => !value,
        crate::EvalScoreValue::Categorical(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{build_evaluators, score_is_failure};
    use crate::assertion::GateEffect;
    use crate::{EvalScore, EvalScoreValue, EvaluatorOptions};

    #[test]
    fn a_diagnostic_score_never_gates_however_low_it_is() {
        // Pins: path similarity is reported, not enforced. A 0.0 diagnostic must
        // not fail a run that satisfied every typed assertion.
        let diagnostic = EvalScore::diagnostic(
            "trajectory_match",
            "trajectory_path_similarity",
            EvalScoreValue::Numeric(0.0),
            None,
        );
        let gating = EvalScore::gating(
            "threshold",
            "cost_within_budget",
            EvalScoreValue::Boolean(false),
            None,
        );

        assert!(!score_is_failure(&diagnostic));
        assert!(score_is_failure(&gating));
        assert_eq!(diagnostic.gate, GateEffect::Diagnostic);
    }

    #[test]
    fn an_unknown_evaluator_name_is_rejected() {
        let outcome = build_evaluators(
            &["definitely_not_registered".to_string()],
            &EvaluatorOptions::default(),
        );

        let Err(error) = outcome else {
            panic!("an unknown evaluator name must be rejected");
        };
        assert!(error.to_string().contains("unknown evaluator"));
    }
}
