//! Dry-run planning and coarse cost estimation for eval suite execution.

use moa_core::MoaConfig;
use moa_providers::{build_provider_from_selection, resolve_provider_selection};
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

/// Builds a dry-run execution plan for one suite and a set of agent configs.
pub(crate) fn build_eval_plan(
    base_config: &MoaConfig,
    suite: &TestSuite,
    configs: &[AgentConfig],
) -> EvalPlan {
    let mut estimated_min = 0.0;
    let mut estimated_max = 0.0;

    for config in configs {
        for case in &suite.cases {
            let (min_cost, max_cost) = estimate_run_cost_range(base_config, config, case);
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

fn estimate_run_cost_range(
    base_config: &MoaConfig,
    config: &AgentConfig,
    case: &TestCase,
) -> (f64, f64) {
    let Ok(selection) = resolve_provider_selection(base_config, config.model.as_deref()) else {
        return (0.0, 0.0);
    };
    let Ok(provider) = build_provider_from_selection(base_config, &selection) else {
        return (0.0, 0.0);
    };
    let pricing = provider.capabilities().pricing;
    let prompt_tokens = estimate_tokens(&case.input).max(128);
    let min_output_tokens = 128usize.min(provider.capabilities().max_output);
    let max_output_tokens = provider.capabilities().max_output.clamp(256, 2_048);

    (
        price_for_tokens(&pricing, prompt_tokens, min_output_tokens),
        price_for_tokens(&pricing, prompt_tokens.saturating_mul(4), max_output_tokens),
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
    pricing: &moa_core::TokenPricing,
    input_tokens: usize,
    output_tokens: usize,
) -> f64 {
    ((input_tokens as f64 * pricing.input_per_mtok)
        + (output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use crate::{AgentConfig, TestCase, TestSuite, plan::build_eval_plan};

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

        let plan = build_eval_plan(&MoaConfig::default(), &suite, &configs);
        assert_eq!(plan.total_runs, 4);
        assert_eq!(plan.configs, vec!["baseline", "variant"]);
        assert_eq!(plan.cases, vec!["case-a", "case-b"]);
    }
}
