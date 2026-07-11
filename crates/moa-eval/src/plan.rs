//! Provider-aware dry-run planning for eval suite execution.

use moa_core::config::MoaConfig;
use moa_eval_core::{
    AgentConfig, EvalPlan, TestCase, TestSuite, build_eval_plan_with_estimator,
    estimate_run_cost_range,
};
use moa_providers::{build_provider_from_selection, resolve_provider_selection};

/// Builds a dry-run plan after resolving each agent configuration's provider
/// and model pricing.
#[must_use]
pub fn build_eval_plan(
    base_config: &MoaConfig,
    suite: &TestSuite,
    configs: &[AgentConfig],
) -> EvalPlan {
    build_eval_plan_with_estimator(suite, configs, |config, case| {
        estimate_provider_run_cost(base_config, config, case)
    })
}

fn estimate_provider_run_cost(
    base_config: &MoaConfig,
    config: &AgentConfig,
    case: &TestCase,
) -> (f64, f64) {
    let Ok((provider_id, model_id)) =
        resolve_provider_selection(base_config, config.model.as_deref())
    else {
        return (0.0, 0.0);
    };
    let Ok(provider) = build_provider_from_selection(base_config, provider_id, &model_id) else {
        return (0.0, 0.0);
    };
    let capabilities = provider.capabilities();
    estimate_run_cost_range(&capabilities.pricing, capabilities.max_output, &case.input)
}
