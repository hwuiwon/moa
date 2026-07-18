//! Hermetic live execution-eval budget and repeated-run aggregation tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use moa_core::types::execution_planning::{
    ExecutionRouteClassifierOutcome, ExecutionRouteKind, ExecutionRouteProvenanceV1,
    ExecutionRouteSource, ExecutionRouteUsageV1, ExecutionStrategy,
};
use moa_eval::execution::{
    EXECUTION_LIVE_CASE_COUNT, EXECUTION_LIVE_REPETITIONS, ExecutionEvalCaseResultV1,
    ExecutionEvalProviderV1, ExecutionJudgeCalibrationStatusV1, ExecutionLiveRunOutcomeV1,
    ExecutionRoutingLabelV1, ExecutionTaskQualityCaseV1, aggregate_live_execution_outcomes,
    forecast_live_execution_cost,
};
use moa_execution::state::ExecutionRunStatus;

fn cases() -> Vec<ExecutionTaskQualityCaseV1> {
    let required_tags = [
        "respond",
        "near-boundary-inline",
        "durable-execute",
        "bulk-coverage",
        "evidence-citations",
        "exclusions",
        "honest-partial",
        "sp500-ai-five-year-screen",
    ];
    (0..EXECUTION_LIVE_CASE_COUNT)
        .map(|index| {
            let (expected_route, expected_strategy) = match index {
                0 => (ExecutionRoutingLabelV1::Respond, None),
                1 => (
                    ExecutionRoutingLabelV1::Execute,
                    Some(ExecutionStrategy::Inline),
                ),
                _ => (
                    ExecutionRoutingLabelV1::Execute,
                    Some(ExecutionStrategy::Durable),
                ),
            };
            let is_durable = expected_strategy == Some(ExecutionStrategy::Durable);
            let mut tags = vec![required_tags[index.min(required_tags.len() - 1)].to_string()];
            if index == 1 {
                tags.push("near-boundary-inline".to_string());
            }
            ExecutionTaskQualityCaseV1 {
                schema_version: 1,
                case_id: format!("live-{index:03}"),
                objective: format!("live objective {index}"),
                expected_route,
                expected_strategy,
                allowed_terminal_statuses: is_durable
                    .then_some(vec![ExecutionRunStatus::Completed])
                    .unwrap_or_default(),
                min_task_count: u64::from(is_durable),
                max_task_count: u64::from(is_durable),
                reference_task_count: u64::from(is_durable),
                contract_case_id: is_durable.then(|| format!("contract-{index:03}")),
                final_message_rubric: "State what was and was not completed.".to_string(),
                estimated_input_tokens_per_run: 2_000,
                estimated_output_tokens_per_run: 1_000,
                seed: 10_000 + index as u64,
                tags,
            }
        })
        .collect()
}

fn provenance(index: usize) -> ExecutionRouteProvenanceV1 {
    ExecutionRouteProvenanceV1 {
        source: ExecutionRouteSource::Classifier,
        classifier_outcome: ExecutionRouteClassifierOutcome::Accepted,
        provider_model: Some("small-live-router".to_string()),
        prompt_version: Some("execution-router-v1".to_string()),
        objective_hash: format!("{index:064x}"),
        response_hash: Some(format!("{:064x}", index + 100)),
        confidence_bps: Some(9_500),
        missing_input_count: 0,
        usage: ExecutionRouteUsageV1 {
            input_tokens_uncached: 100,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 20,
        },
        cost_microusd: 10,
        duration_micros: 2_000,
    }
}

fn outcomes(cases: &[ExecutionTaskQualityCaseV1]) -> Vec<ExecutionLiveRunOutcomeV1> {
    cases
        .iter()
        .enumerate()
        .flat_map(|(index, case)| {
            (1..=EXECUTION_LIVE_REPETITIONS).map(move |repetition| {
                let is_durable = case.expected_strategy == Some(ExecutionStrategy::Durable);
                let observed_route = case.expected_route;
                let observed_strategy = case.expected_strategy;
                let task_count = u64::from(is_durable);
                ExecutionLiveRunOutcomeV1 {
                    case_id: case.case_id.clone(),
                    repetition,
                    observed_route,
                    observed_strategy,
                    result: ExecutionEvalCaseResultV1 {
                        case_id: format!("{}#run={repetition}", case.case_id),
                        passed: true,
                        contract_omission: is_durable.then_some(false),
                        contract_score: is_durable.then_some(1.0),
                        impossible_case: false,
                        execution_false_completion: false,
                        observed_run_status: is_durable.then_some(ExecutionRunStatus::Completed),
                        observed_route: match observed_route {
                            ExecutionRoutingLabelV1::Respond => Some(ExecutionRouteKind::Respond),
                            ExecutionRoutingLabelV1::Execute => Some(ExecutionRouteKind::Execute),
                            ExecutionRoutingLabelV1::NeedsInput => {
                                Some(ExecutionRouteKind::NeedsInput)
                            }
                        },
                        observed_strategy,
                        route_provenance: Some(provenance(index)),
                        invariants: Vec::new(),
                        cost_microusd: 100,
                        latency_ms: 1_000,
                        task_count,
                        terminal_output_hash: None,
                        final_response_hash: None,
                    },
                }
            })
        })
        .collect()
}

#[test]
fn execution_live_budget_refuses_before_dispatch_offline() {
    // Pins: the complete 100-call forecast is authorized before any provider callback runs.
    let dispatches = AtomicU64::new(0);
    let cases = cases();

    let refused = forecast_live_execution_cost(&cases, EXECUTION_LIVE_REPETITIONS, 0.000_001);
    if refused.is_ok() {
        dispatches.fetch_add(1, Ordering::Relaxed);
    }

    assert!(refused.is_err());
    assert_eq!(dispatches.load(Ordering::Relaxed), 0);
    let accepted = forecast_live_execution_cost(&cases, EXECUTION_LIVE_REPETITIONS, 10.0)
        .expect("sufficient budget should authorize the full batch");
    assert_eq!(accepted.run_count, 100);
    assert!(accepted.ledger.est_usd > 0.0);
}

#[test]
fn execution_live_aggregation_requires_five_independent_outcomes_and_reports_reliability_offline() {
    // Pins: pass@1 and pass-all-five use all persisted independent rows, not one sample.
    let cases = cases();
    let mut outcomes = outcomes(&cases);
    let mut hashes = BTreeMap::new();
    hashes.insert("task_quality".to_string(), "a".repeat(64));
    let report = aggregate_live_execution_outcomes(
        &cases,
        &outcomes,
        EXECUTION_LIVE_REPETITIONS,
        hashes.clone(),
        ExecutionJudgeCalibrationStatusV1::Unavailable,
        ExecutionEvalProviderV1 {
            provider: "live".to_string(),
            model: "configured".to_string(),
            prompt_version: "execution-planner-v1".to_string(),
        },
    )
    .expect("complete repeated outcomes should aggregate");

    assert_eq!(report.cases.len(), 100);
    assert_eq!(report.repetitions, 5);
    assert_eq!(report.metrics.pass_at_1, Some(1.0));
    assert_eq!(report.metrics.pass_all_k, Some(1.0));
    assert_eq!(report.metrics.pass_variance, Some(0.0));
    assert_eq!(report.metrics.respond_on_execute_rate, Some(0.0));
    assert_eq!(report.metrics.weighted_routing_cost, Some(0.0));
    assert_eq!(report.metrics.weighted_strategy_cost, Some(0.0));
    assert_eq!(report.metrics.durable_strategy_recall, Some(1.0));
    assert_eq!(report.metrics.classifier_fallback_rate, Some(0.0));
    assert_eq!(report.metrics.task_count_ratio_vs_reference, Some(1.0));
    assert_eq!(
        report.judge_calibration_status,
        ExecutionJudgeCalibrationStatusV1::Unavailable
    );

    outcomes.pop();
    assert!(
        aggregate_live_execution_outcomes(
            &cases,
            &outcomes,
            EXECUTION_LIVE_REPETITIONS,
            hashes,
            ExecutionJudgeCalibrationStatusV1::Unavailable,
            ExecutionEvalProviderV1 {
                provider: "live".to_string(),
                model: "configured".to_string(),
                prompt_version: "execution-planner-v1".to_string(),
            },
        )
        .is_err()
    );
}
