//! Dry-run planning and provider-independent cost arithmetic for eval execution.

use moa_core::types::model::TokenPricing;
use serde::{Deserialize, Serialize};

use crate::{AgentConfig, TestCase, TestSuite};

/// Preview of an eval suite run without executing any LLM calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EvalPlan {
    /// Suite name that would be executed.
    pub suite_name: String,
    /// Agent config names included in the run.
    pub configs: Vec<String>,
    /// Test case names included in the run.
    pub cases: Vec<String>,
    /// Total `(config, case)` executions.
    pub total_runs: usize,
    /// Coarse minimum and maximum estimated dollar cost.
    pub estimated_cost_range: (f64, f64),
}

/// Builds a dry-run execution plan using an owning-layer cost estimator.
///
/// Provider selection stays outside `moa-eval-core`; this function owns only
/// matrix enumeration and deterministic aggregation.
pub fn build_eval_plan_with_estimator(
    suite: &TestSuite,
    configs: &[AgentConfig],
    mut estimate: impl FnMut(&AgentConfig, &TestCase) -> (f64, f64),
) -> EvalPlan {
    let mut estimated_min = 0.0;
    let mut estimated_max = 0.0;

    for config in configs {
        for case in &suite.cases {
            let (min_cost, max_cost) = estimate(config, case);
            estimated_min += min_cost;
            estimated_max += max_cost;
        }
    }

    EvalPlan {
        suite_name: suite.name.clone(),
        configs: configs.iter().map(|config| config.name.clone()).collect(),
        cases: suite.cases.iter().map(|case| case.name.clone()).collect(),
        total_runs: configs.len() * suite.cases.len(),
        estimated_cost_range: (estimated_min, estimated_max),
    }
}

/// Estimates the coarse cost range for one test case from resolved model
/// pricing and output capacity.
#[must_use]
pub fn estimate_run_cost_range(
    pricing: &TokenPricing,
    max_output: usize,
    input: &str,
) -> (f64, f64) {
    let prompt_tokens = estimate_tokens(input).max(128);
    let min_output_tokens = 128usize.min(max_output);
    let max_output_tokens = max_output.clamp(256, 2_048);

    (
        price_for_tokens(pricing, prompt_tokens, min_output_tokens),
        price_for_tokens(pricing, prompt_tokens.saturating_mul(4), max_output_tokens),
    )
}

fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

fn price_for_tokens(
    pricing: &moa_core::types::model::TokenPricing,
    input_tokens: usize,
    output_tokens: usize,
) -> f64 {
    ((input_tokens as f64 * pricing.input_per_mtok)
        + (output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use moa_core::types::model::TokenPricing;

    use crate::{
        AgentConfig, TestCase, TestSuite,
        plan::{build_eval_plan_with_estimator, estimate_run_cost_range},
    };

    #[test]
    fn plan_counts_all_runs() {
        let suite = TestSuite {
            name: "suite".to_string(),
            cases: vec![
                TestCase {
                    name: "case-a".to_string(),
                    input: "hello".to_string(),
                    ..TestCase::default()
                },
                TestCase {
                    name: "case-b".to_string(),
                    input: "world".to_string(),
                    ..TestCase::default()
                },
            ],
            ..TestSuite::default()
        };
        let configs = vec![
            AgentConfig {
                name: "baseline".to_string(),
                ..AgentConfig::default()
            },
            AgentConfig {
                name: "variant".to_string(),
                ..AgentConfig::default()
            },
        ];

        let plan = build_eval_plan_with_estimator(&suite, &configs, |config, case| {
            let multiplier = if config.name == "variant" { 2.0 } else { 1.0 };
            let base = if case.name == "case-b" { 0.02 } else { 0.01 };
            (base * multiplier, base * multiplier * 4.0)
        });
        assert_eq!(plan.total_runs, 4);
        assert_eq!(plan.configs, vec!["baseline", "variant"]);
        assert_eq!(plan.cases, vec!["case-a", "case-b"]);
        assert_eq!(plan.estimated_cost_range, (0.09, 0.36));
    }

    #[test]
    fn cost_range_uses_resolved_pricing_and_output_limits() {
        // Pins: the core cost arithmetic remains deterministic after provider
        // selection moves to the owning eval crate.
        let pricing = TokenPricing {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cached_input_per_mtok: None,
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        };

        let (minimum, maximum) = estimate_run_cost_range(&pricing, 1_024, "hello");

        assert_eq!(minimum, 0.000384);
        assert_eq!(maximum, 0.00256);
    }
}
