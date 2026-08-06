//! Budgeted repeated-run contracts for the sampled live execution-eval lane.

use std::collections::{BTreeMap, BTreeSet};

use moa_core::types::execution_planning::{
    ExecutionRouteClassifierOutcome, ExecutionRouteKind, ExecutionRouteSource, ExecutionStrategy,
};
use moa_eval_core::reliability::{
    CaseReliabilityReport, CaseTrials, ReliabilityProvenance, RepetitionIntegrity,
    RepetitionObservation, TrialIndependence, estimate_reliability, group_repetitions,
    pooled_outcome_variance,
};
use moa_eval_core::{Error, Result};
use moa_execution::state::ExecutionRunStatus;
use serde::{Deserialize, Serialize};

use crate::kernel::CostLedger;

use super::{
    report::{
        ExecutionEvalCaseResult, ExecutionEvalLane, ExecutionEvalProvider, ExecutionEvalReport,
        ExecutionJudgeCalibrationStatus,
    },
    routing::{ExecutionRoutingLabel, routing_cost, strategy_cost},
};

/// Required number of logical cases in the initial live execution corpus.
pub const EXECUTION_LIVE_CASE_COUNT: usize = 20;
/// Required number of independent provider outcomes per live case.
pub const EXECUTION_LIVE_REPETITIONS: u32 = 5;

/// One strict live routing, planner, and task-quality case.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskQualityCase {
    /// Case schema version, fixed at `1`.
    pub schema_version: u8,
    /// Stable logical case identifier.
    pub case_id: String,
    /// Exact user objective sent to an independent session.
    pub objective: String,
    /// Human-adjudicated route required before planner/task scoring.
    pub expected_route: ExecutionRoutingLabel,
    /// Human-adjudicated strategy, present only for Execute.
    pub expected_strategy: Option<ExecutionStrategy>,
    /// Allowed durable terminal statuses; empty for non-Durable cases.
    pub allowed_terminal_statuses: Vec<ExecutionRunStatus>,
    /// Inclusive minimum persisted logical-task count.
    pub min_task_count: u64,
    /// Inclusive maximum persisted logical-task count.
    pub max_task_count: u64,
    /// Human reference-plan task count used only for efficiency reporting.
    pub reference_task_count: u64,
    /// Optional recorded contract case whose gold expectations apply to the generated candidate.
    pub contract_case_id: Option<String>,
    /// Human-readable rubric reserved for a calibrated semantic judge.
    pub final_message_rubric: String,
    /// Provider-neutral input-token forecast per independent run.
    pub estimated_input_tokens_per_run: u64,
    /// Provider-neutral output-token forecast per independent run.
    pub estimated_output_tokens_per_run: u64,
    /// Fixed corpus seed retained in reports.
    pub seed: u64,
    /// Stable coverage labels.
    pub tags: Vec<String>,
}

/// Provider-neutral forecast authorized before any live dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLiveCostForecast {
    /// Number of logical corpus cases.
    pub case_count: u64,
    /// Number of independent runs per logical case.
    pub repetitions: u32,
    /// Total provider calls represented by the forecast.
    pub run_count: u64,
    /// Existing shared eval-cost ledger after all calls are forecast.
    pub ledger: CostLedger,
}

/// One persisted independent live outcome ready for report aggregation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLiveRunOutcome {
    /// Logical task-quality case identifier.
    pub case_id: String,
    /// One-based independent repetition number.
    pub repetition: u32,
    /// Observed final routing label, including NeedsInput.
    pub observed_route: ExecutionRoutingLabel,
    /// Observed deterministic strategy, present only for Execute.
    pub observed_strategy: Option<ExecutionStrategy>,
    /// Common redacted execution-eval case result.
    pub result: ExecutionEvalCaseResult,
}

/// Validates one task-quality case independently of corpus-level cardinality.
pub(crate) fn validate_task_quality_case(case: &ExecutionTaskQualityCase) -> Result<()> {
    if case.schema_version != 1
        || case.case_id.trim().is_empty()
        || case.objective.trim().is_empty()
        || case.final_message_rubric.trim().is_empty()
        || case.tags.is_empty()
        || case.estimated_input_tokens_per_run == 0
        || case.estimated_output_tokens_per_run == 0
        || case.min_task_count > case.max_task_count
    {
        return Err(invalid_config(format!(
            "execution task-quality case `{}` has an invalid required field or task bound",
            case.case_id
        )));
    }
    if case
        .allowed_terminal_statuses
        .iter()
        .enumerate()
        .any(|(index, status)| case.allowed_terminal_statuses[..index].contains(status))
    {
        return Err(invalid_config(format!(
            "execution task-quality case `{}` repeats a terminal status",
            case.case_id
        )));
    }
    if case
        .allowed_terminal_statuses
        .iter()
        .any(|status| !status.is_terminal())
    {
        return Err(invalid_config(format!(
            "execution task-quality case `{}` lists a nonterminal run status as terminal",
            case.case_id
        )));
    }
    match (case.expected_route, case.expected_strategy) {
        (ExecutionRoutingLabel::Execute, Some(ExecutionStrategy::Durable)) => {
            if case.allowed_terminal_statuses.is_empty()
                || case.reference_task_count == 0
                || case.max_task_count == 0
            {
                return Err(invalid_config(format!(
                    "Durable Execute task-quality case `{}` requires statuses and positive task references",
                    case.case_id
                )));
            }
        }
        (ExecutionRoutingLabel::Respond | ExecutionRoutingLabel::NeedsInput, None)
        | (ExecutionRoutingLabel::Execute, Some(ExecutionStrategy::Inline)) => {
            if !case.allowed_terminal_statuses.is_empty()
                || case.min_task_count != 0
                || case.max_task_count != 0
                || case.reference_task_count != 0
                || case.contract_case_id.is_some()
            {
                return Err(invalid_config(format!(
                    "non-Durable task-quality case `{}` cannot declare run-only expectations",
                    case.case_id
                )));
            }
        }
        _ => {
            return Err(invalid_config(format!(
                "task-quality case `{}` has an inconsistent route and strategy",
                case.case_id
            )));
        }
    }
    Ok(())
}

/// Validates the exact initial task-quality corpus and its required cohorts.
pub(crate) fn validate_task_quality_corpus(cases: &[ExecutionTaskQualityCase]) -> Result<()> {
    if cases.len() != EXECUTION_LIVE_CASE_COUNT {
        return Err(invalid_config(format!(
            "execution task-quality corpus must contain exactly {EXECUTION_LIVE_CASE_COUNT} cases"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut seeds = BTreeSet::new();
    let mut tags = BTreeSet::new();
    for case in cases {
        validate_task_quality_case(case)?;
        if !ids.insert(case.case_id.as_str()) || !seeds.insert(case.seed) {
            return Err(invalid_config(
                "execution task-quality case IDs and seeds must be unique".to_string(),
            ));
        }
        tags.extend(case.tags.iter().map(String::as_str));
    }
    for required in [
        "respond",
        "near-boundary-inline",
        "durable-execute",
        "bulk-coverage",
        "evidence-citations",
        "exclusions",
        "honest-partial",
        "sp500-ai-five-year-screen",
    ] {
        if !tags.contains(required) {
            return Err(invalid_config(format!(
                "execution task-quality corpus is missing required `{required}` coverage"
            )));
        }
    }
    Ok(())
}

/// Forecasts all repeated provider work and rejects over-budget runs before dispatch.
pub fn forecast_live_execution_cost(
    cases: &[ExecutionTaskQualityCase],
    repetitions: u32,
    budget_usd: f64,
) -> Result<ExecutionLiveCostForecast> {
    validate_task_quality_corpus(cases)?;
    if repetitions == 0 || !budget_usd.is_finite() || budget_usd <= 0.0 {
        return Err(invalid_config(
            "live execution repetitions and budget-usd must be positive and finite".to_string(),
        ));
    }
    let repetitions_u64 = u64::from(repetitions);
    let mut ledger = CostLedger::new(budget_usd);
    for case in cases {
        let input = case
            .estimated_input_tokens_per_run
            .checked_mul(repetitions_u64)
            .ok_or_else(|| {
                invalid_config("live input-token forecast overflowed u64".to_string())
            })?;
        let output = case
            .estimated_output_tokens_per_run
            .checked_mul(repetitions_u64)
            .ok_or_else(|| {
                invalid_config("live output-token forecast overflowed u64".to_string())
            })?;
        ledger.record_chat(input, output);
    }
    ledger.check_budget()?;
    let case_count = u64::try_from(cases.len())
        .map_err(|_| invalid_config("live case count exceeds u64".to_string()))?;
    let run_count = case_count
        .checked_mul(repetitions_u64)
        .ok_or_else(|| invalid_config("live run count overflowed u64".to_string()))?;
    Ok(ExecutionLiveCostForecast {
        case_count,
        repetitions,
        run_count,
        ledger,
    })
}

/// Aggregates exactly `k` independent outcomes per logical live case into one strict report.
pub fn aggregate_live_execution_outcomes(
    cases: &[ExecutionTaskQualityCase],
    outcomes: &[ExecutionLiveRunOutcome],
    repetitions: u32,
    corpus_hashes: BTreeMap<String, String>,
    calibration_status: ExecutionJudgeCalibrationStatus,
    provider: ExecutionEvalProvider,
) -> Result<ExecutionEvalReport> {
    validate_task_quality_corpus(cases)?;
    if repetitions != EXECUTION_LIVE_REPETITIONS {
        return Err(invalid_config(format!(
            "live execution aggregation requires exactly {EXECUTION_LIVE_REPETITIONS} repetitions"
        )));
    }
    let expected_count = cases
        .len()
        .checked_mul(repetitions as usize)
        .ok_or_else(|| invalid_config("live outcome count overflowed usize".to_string()))?;
    if outcomes.len() != expected_count {
        return Err(invalid_config(format!(
            "live execution aggregation expected {expected_count} outcomes, got {}",
            outcomes.len()
        )));
    }
    let case_by_id = cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect::<BTreeMap<_, _>>();
    let mut identities = BTreeSet::new();
    let mut report_cases = Vec::with_capacity(outcomes.len());
    let mut total_routing_cost = 0_u64;
    let mut total_strategy_cost = 0_u64;
    let mut strategy_cost_cases = 0_u64;
    let mut execute_cases = 0_u64;
    let mut respond_on_execute = 0_u64;
    let mut durable_strategy_cases = 0_u64;
    let mut durable_strategy_correct = 0_u64;
    let mut near_boundary_inline = 0_u64;
    let mut near_boundary_inline_correct = 0_u64;
    let mut classifier_attempts = 0_u64;
    let mut classifier_fallbacks = 0_u64;
    let mut fallback_counts = BTreeMap::<String, u64>::new();
    let mut classifier_tokens = 0_u64;
    let mut classifier_cost = 0_u64;
    let mut classifier_duration_micros = 0_u64;
    let mut total_cost = 0_u64;
    let mut successful = 0_u64;
    let mut task_count = 0_u64;
    let mut reference_task_count = 0_u64;

    for outcome in outcomes {
        let case = case_by_id.get(outcome.case_id.as_str()).ok_or_else(|| {
            invalid_config(format!(
                "live outcome references unknown case `{}`",
                outcome.case_id
            ))
        })?;
        if outcome.repetition == 0
            || outcome.repetition > repetitions
            || !identities.insert((outcome.case_id.as_str(), outcome.repetition))
        {
            return Err(invalid_config(format!(
                "live outcome `{}` has an invalid or duplicate repetition {}",
                outcome.case_id, outcome.repetition
            )));
        }
        let expected_result_id = format!("{}#run={}", outcome.case_id, outcome.repetition);
        if outcome.result.case_id != expected_result_id {
            return Err(invalid_config(format!(
                "live outcome case result must use identity `{expected_result_id}`"
            )));
        }
        if outcome.result.observed_route != Some(label_kind(outcome.observed_route))
            || outcome.result.observed_strategy != outcome.observed_strategy
        {
            return Err(invalid_config(format!(
                "live outcome `{expected_result_id}` route label and common result disagree"
            )));
        }
        let route_passed = outcome.observed_route == case.expected_route
            && outcome.observed_strategy == case.expected_strategy;
        let status_passed = match (case.expected_route, case.expected_strategy) {
            (ExecutionRoutingLabel::Execute, Some(ExecutionStrategy::Durable)) => outcome
                .result
                .observed_run_status
                .is_some_and(|status| case.allowed_terminal_statuses.contains(&status)),
            _ => outcome.result.observed_run_status.is_none(),
        };
        let task_passed = outcome.result.task_count >= case.min_task_count
            && outcome.result.task_count <= case.max_task_count;
        let structurally_passed = route_passed
            && status_passed
            && task_passed
            && !outcome.result.execution_false_completion
            && outcome.result.contract_omission != Some(true)
            && outcome.result.invariants.iter().all(|result| result.passed);
        if outcome.result.passed != structurally_passed {
            return Err(invalid_config(format!(
                "live outcome `{expected_result_id}` pass bit disagrees with structured expectations"
            )));
        }
        successful = successful.saturating_add(u64::from(outcome.result.passed));
        total_cost = total_cost
            .checked_add(outcome.result.cost_microusd)
            .ok_or_else(|| invalid_config("live cost aggregate overflowed u64".to_string()))?;
        task_count = task_count
            .checked_add(outcome.result.task_count)
            .ok_or_else(|| invalid_config("live task aggregate overflowed u64".to_string()))?;
        reference_task_count = reference_task_count
            .checked_add(case.reference_task_count)
            .ok_or_else(|| {
                invalid_config("live reference-task aggregate overflowed u64".to_string())
            })?;
        total_routing_cost = total_routing_cost
            .checked_add(routing_cost(case.expected_route, outcome.observed_route))
            .ok_or_else(|| invalid_config("live routing cost overflowed u64".to_string()))?;
        if let Some(cost) = strategy_cost(
            case.expected_route,
            case.expected_strategy,
            outcome.observed_route,
            outcome.observed_strategy,
        ) {
            total_strategy_cost = total_strategy_cost
                .checked_add(cost)
                .ok_or_else(|| invalid_config("live strategy cost overflowed u64".to_string()))?;
            strategy_cost_cases = strategy_cost_cases.saturating_add(1);
        }
        if case.expected_route == ExecutionRoutingLabel::Execute {
            execute_cases = execute_cases.saturating_add(1);
            respond_on_execute = respond_on_execute.saturating_add(u64::from(
                outcome.observed_route == ExecutionRoutingLabel::Respond,
            ));
            if case.expected_strategy == Some(ExecutionStrategy::Durable) {
                durable_strategy_cases = durable_strategy_cases.saturating_add(1);
                durable_strategy_correct = durable_strategy_correct.saturating_add(u64::from(
                    outcome.observed_route == ExecutionRoutingLabel::Execute
                        && outcome.observed_strategy == Some(ExecutionStrategy::Durable),
                ));
            }
        }
        if case.tags.iter().any(|tag| tag == "near-boundary-inline") {
            near_boundary_inline = near_boundary_inline.saturating_add(1);
            near_boundary_inline_correct = near_boundary_inline_correct.saturating_add(u64::from(
                outcome.observed_route == ExecutionRoutingLabel::Execute
                    && outcome.observed_strategy == Some(ExecutionStrategy::Inline),
            ));
        }
        if let Some(provenance) = &outcome.result.route_provenance
            && provenance.source == ExecutionRouteSource::Classifier
        {
            classifier_attempts = classifier_attempts.saturating_add(1);
            let route_tokens = super::route_token_total(provenance.usage).ok_or_else(|| {
                invalid_config("live route token total overflowed u64".to_string())
            })?;
            classifier_tokens = classifier_tokens.checked_add(route_tokens).ok_or_else(|| {
                invalid_config("live classifier token overflowed u64".to_string())
            })?;
            classifier_cost = classifier_cost
                .checked_add(provenance.cost_microusd)
                .ok_or_else(|| invalid_config("live classifier cost overflowed u64".to_string()))?;
            classifier_duration_micros = classifier_duration_micros
                .checked_add(provenance.duration_micros)
                .ok_or_else(|| {
                    invalid_config("live classifier duration overflowed u64".to_string())
                })?;
            if provenance.classifier_outcome != ExecutionRouteClassifierOutcome::Accepted {
                classifier_fallbacks = classifier_fallbacks.saturating_add(1);
                *fallback_counts
                    .entry(classifier_outcome_label(provenance.classifier_outcome).to_string())
                    .or_default() += 1;
            }
        }
        report_cases.push(outcome.result.clone());
    }

    let seeds = cases.iter().map(|case| case.seed).collect::<Vec<_>>();
    let mut report = ExecutionEvalReport::new(
        ExecutionEvalLane::NightlyLive,
        corpus_hashes,
        seeds,
        repetitions,
        calibration_status,
        Some(provider),
        report_cases,
    )?;
    let total = report.metrics.total_cases;
    // Reliability is estimated per logical case and then averaged across cases; the
    // shared estimators never pool successes over different cases.
    let (trials, integrity) = live_case_trials(cases, outcomes, repetitions)?;
    require_complete_repetitions(&integrity)?;
    let single_draw = estimate_reliability(&trials, 1, TrialIndependence::IndependentRepetitions)?;
    let full_draw = estimate_reliability(
        &trials,
        repetitions,
        TrialIndependence::IndependentRepetitions,
    )?;
    report.metrics.pass_at_1 = Some(single_draw.pass_any_at_k);
    report.metrics.pass_all_k = Some(full_draw.pass_all_at_k);
    report.metrics.pass_variance = pooled_outcome_variance(&trials);
    report.metrics.cost_per_success_microusd = ratio(total_cost, successful);
    report.metrics.task_count_ratio_vs_reference = ratio(task_count, reference_task_count);
    report.metrics.weighted_routing_cost = ratio(total_routing_cost, total);
    report.metrics.weighted_strategy_cost = ratio(total_strategy_cost, strategy_cost_cases);
    report.metrics.respond_on_execute_rate = ratio(respond_on_execute, execute_cases);
    report.metrics.near_boundary_inline_recall =
        ratio(near_boundary_inline_correct, near_boundary_inline);
    report.metrics.durable_strategy_recall =
        ratio(durable_strategy_correct, durable_strategy_cases);
    report.metrics.classifier_fallback_rate = ratio(classifier_fallbacks, classifier_attempts);
    report.metrics.classifier_fallback_counts =
        (!fallback_counts.is_empty()).then_some(fallback_counts);
    report.metrics.classifier_tokens_per_routed_turn =
        ratio(classifier_tokens, classifier_attempts);
    report.metrics.classifier_cost_microusd_per_routed_turn =
        ratio(classifier_cost, classifier_attempts);
    report.metrics.classifier_latency_ms_per_routed_turn =
        ratio(classifier_duration_micros, classifier_attempts).map(|value| value / 1_000.0);
    report.validate()?;
    Ok(report)
}

/// Builds the standalone case-level repeat-reliability report for a live run.
///
/// The persisted [`ExecutionEvalReport`] carries only the two decision points the
/// lane gates on, `pass_at_1` and `pass_all_k`. This function emits the full
/// `k = 1..=repetitions` curve alongside repetition bookkeeping, corpus seeds,
/// model provenance, and cost completeness, so retry economics can be read without
/// changing the gated report schema.
///
/// Every repetition is an independent re-run of the same objective, which is what
/// licenses the combinatorial estimators. Shared-prefix or branched rollouts must
/// not be fed here.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when the corpus is invalid, when `repetitions`
/// is zero, when an outcome names an unknown case, or when a declared case produced
/// no usable repetition at all. Missing and duplicate repetitions for a case that
/// still has usable rows are reported in [`RepetitionIntegrity`] rather than
/// refused, and simply shorten the curve.
pub fn live_execution_reliability(
    cases: &[ExecutionTaskQualityCase],
    outcomes: &[ExecutionLiveRunOutcome],
    repetitions: u32,
    provider: ExecutionEvalProvider,
) -> Result<CaseReliabilityReport> {
    validate_task_quality_corpus(cases)?;
    if repetitions == 0 {
        return Err(invalid_config(
            "live execution reliability requires positive repetitions".to_string(),
        ));
    }
    let (trials, integrity) = live_case_trials(cases, outcomes, repetitions)?;
    if trials.len() != cases.len() {
        return Err(invalid_config(format!(
            "live execution reliability lost {} declared case(s) with no usable repetition",
            cases.len() - trials.len()
        )));
    }
    let mut cost_microusd = 0_u64;
    let mut cost_complete = !outcomes.is_empty();
    for outcome in outcomes {
        cost_microusd = cost_microusd
            .checked_add(outcome.result.cost_microusd)
            .ok_or_else(|| invalid_config("live reliability cost overflowed u64".to_string()))?;
        cost_complete &= outcome.result.cost_microusd > 0;
    }
    CaseReliabilityReport::new(
        trials,
        TrialIndependence::IndependentRepetitions,
        integrity,
        ReliabilityProvenance {
            provider: provider.provider,
            model: provider.model,
            prompt_version: provider.prompt_version,
            seeds: cases.iter().map(|case| case.seed).collect(),
            cost_microusd,
            cost_complete,
        },
    )
}

/// Groups live repetition rows into per-case trial counts with their integrity record.
fn live_case_trials(
    cases: &[ExecutionTaskQualityCase],
    outcomes: &[ExecutionLiveRunOutcome],
    repetitions: u32,
) -> Result<(Vec<CaseTrials>, RepetitionIntegrity)> {
    let case_ids = cases
        .iter()
        .map(|case| case.case_id.as_str())
        .collect::<Vec<_>>();
    let observations = outcomes
        .iter()
        .map(|outcome| RepetitionObservation {
            case_id: outcome.case_id.clone(),
            repetition: outcome.repetition,
            passed: outcome.result.passed,
        })
        .collect::<Vec<_>>();
    group_repetitions(&case_ids, repetitions, &observations)
}

/// Refuses a gated aggregate whose repetitions are missing, duplicated, or unknown.
fn require_complete_repetitions(integrity: &RepetitionIntegrity) -> Result<()> {
    if integrity.is_complete() {
        return Ok(());
    }
    Err(invalid_config(format!(
        "live execution reliability requires every declared repetition: missing={}, \
         duplicate={}, out_of_range={}, unknown_case={}",
        integrity.missing_repetitions,
        integrity.duplicate_repetitions,
        integrity.out_of_range_repetitions,
        integrity.unknown_case_rows
    )))
}

const fn label_kind(label: ExecutionRoutingLabel) -> ExecutionRouteKind {
    match label {
        ExecutionRoutingLabel::Respond => ExecutionRouteKind::Respond,
        ExecutionRoutingLabel::Execute => ExecutionRouteKind::Execute,
        ExecutionRoutingLabel::NeedsInput => ExecutionRouteKind::NeedsInput,
    }
}

const fn classifier_outcome_label(outcome: ExecutionRouteClassifierOutcome) -> &'static str {
    match outcome {
        ExecutionRouteClassifierOutcome::NotCalled => "not_called",
        ExecutionRouteClassifierOutcome::Accepted => "accepted",
        ExecutionRouteClassifierOutcome::ProviderError => "provider_error",
        ExecutionRouteClassifierOutcome::StreamError => "stream_error",
        ExecutionRouteClassifierOutcome::Oversized => "oversized",
        ExecutionRouteClassifierOutcome::SchemaRejected => "schema_rejected",
        ExecutionRouteClassifierOutcome::InvalidDecision => "invalid_decision",
        ExecutionRouteClassifierOutcome::LowConfidence => "low_confidence",
        ExecutionRouteClassifierOutcome::ContextForcedInline => "context_forced_inline",
    }
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator != 0).then_some(numerator as f64 / denominator as f64)
}

fn invalid_config(message: String) -> Error {
    Error::InvalidConfig(message)
}

#[cfg(test)]
mod tests {
    use moa_core::types::execution_planning::{
        ExecutionRouteProvenance, ExecutionRouteSource, ExecutionRouteUsage,
    };
    use moa_eval_core::reliability::ReliabilityResamplingUnit;

    use super::*;

    fn corpus() -> Vec<ExecutionTaskQualityCase> {
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
                    0 => (ExecutionRoutingLabel::Respond, None),
                    1 => (
                        ExecutionRoutingLabel::Execute,
                        Some(ExecutionStrategy::Inline),
                    ),
                    _ => (
                        ExecutionRoutingLabel::Execute,
                        Some(ExecutionStrategy::Durable),
                    ),
                };
                let is_durable = expected_strategy == Some(ExecutionStrategy::Durable);
                let mut tags = vec![required_tags[index.min(required_tags.len() - 1)].to_string()];
                if index == 1 {
                    tags.push("near-boundary-inline".to_string());
                }
                ExecutionTaskQualityCase {
                    schema_version: 1,
                    case_id: format!("live-{index:03}"),
                    objective: format!("live objective {index}"),
                    expected_route,
                    expected_strategy,
                    allowed_terminal_statuses: if is_durable {
                        vec![ExecutionRunStatus::Completed]
                    } else {
                        Vec::new()
                    },
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

    #[test]
    fn compensating_status_is_rejected_from_live_terminal_expectations_offline() {
        // Pins: an in-progress rollback cannot satisfy a live eval's terminal-status contract.
        let mut case = corpus()
            .into_iter()
            .find(|case| case.expected_strategy == Some(ExecutionStrategy::Durable))
            .expect("fixture corpus must contain one durable case");
        case.allowed_terminal_statuses = vec![ExecutionRunStatus::Compensating];

        let error = validate_task_quality_case(&case)
            .expect_err("Compensating must not be accepted as a terminal status");
        let Error::InvalidConfig(message) = error else {
            panic!("expected InvalidConfig for nonterminal status, got {error}");
        };
        assert_eq!(
            message,
            format!(
                "execution task-quality case `{}` lists a nonterminal run status as terminal",
                case.case_id
            )
        );
    }

    fn route_provenance(index: usize) -> ExecutionRouteProvenance {
        ExecutionRouteProvenance {
            source: ExecutionRouteSource::Classifier,
            classifier_outcome: ExecutionRouteClassifierOutcome::Accepted,
            provider_model: Some("small-live-router".to_string()),
            prompt_version: Some("execution-router".to_string()),
            objective_hash: format!("{index:064x}"),
            response_hash: Some(format!("{:064x}", index + 100)),
            confidence_bps: Some(9_500),
            missing_input_count: 0,
            usage: ExecutionRouteUsage {
                input_tokens_uncached: 100,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 20,
            },
            cost_microusd: 10,
            duration_micros: 2_000,
        }
    }

    /// Fails a repetition by observing NeedsInput instead of the adjudicated route,
    /// which is the failure shape the live lane actually sees.
    fn mixed_outcomes(cases: &[ExecutionTaskQualityCase]) -> Vec<ExecutionLiveRunOutcome> {
        cases
            .iter()
            .enumerate()
            .flat_map(|(index, case)| {
                (1..=EXECUTION_LIVE_REPETITIONS).map(move |repetition| {
                    let is_durable = case.expected_strategy == Some(ExecutionStrategy::Durable);
                    let failing = index % 3 != 0 && (index as u32 + repetition).is_multiple_of(4);
                    let observed_route = if failing {
                        ExecutionRoutingLabel::NeedsInput
                    } else {
                        case.expected_route
                    };
                    let observed_strategy = if failing {
                        None
                    } else {
                        case.expected_strategy
                    };
                    ExecutionLiveRunOutcome {
                        case_id: case.case_id.clone(),
                        repetition,
                        observed_route,
                        observed_strategy,
                        result: ExecutionEvalCaseResult {
                            case_id: format!("{}#run={repetition}", case.case_id),
                            passed: !failing,
                            contract_omission: is_durable.then_some(false),
                            contract_score: is_durable.then_some(1.0),
                            impossible_case: false,
                            execution_false_completion: false,
                            observed_run_status: (is_durable && !failing)
                                .then_some(ExecutionRunStatus::Completed),
                            observed_route: Some(label_kind(observed_route)),
                            observed_strategy,
                            route_provenance: Some(route_provenance(index)),
                            invariants: Vec::new(),
                            cost_microusd: 100 + index as u64,
                            latency_ms: 1_000,
                            task_count: u64::from(is_durable && !failing),
                            terminal_output_hash: None,
                            final_response_hash: None,
                        },
                    }
                })
            })
            .collect()
    }

    fn aggregate(
        cases: &[ExecutionTaskQualityCase],
        outcomes: &[ExecutionLiveRunOutcome],
    ) -> Result<ExecutionEvalReport> {
        let mut hashes = BTreeMap::new();
        hashes.insert("task_quality".to_string(), "a".repeat(64));
        aggregate_live_execution_outcomes(
            cases,
            outcomes,
            EXECUTION_LIVE_REPETITIONS,
            hashes,
            ExecutionJudgeCalibrationStatus::Unavailable,
            provider(),
        )
    }

    fn provider() -> ExecutionEvalProvider {
        ExecutionEvalProvider {
            provider: "live".to_string(),
            model: "configured".to_string(),
            prompt_version: "execution-planner".to_string(),
        }
    }

    #[test]
    fn shared_estimators_reproduce_the_pre_extraction_report_arithmetic_offline() {
        // Pins: moving pass_at_1/pass_all_k/pass_variance onto the shared estimators
        // changes no reported bit, including the f64 rounding of the old ratios.
        let cases = corpus();
        let outcomes = mixed_outcomes(&cases);
        let report = aggregate(&cases, &outcomes).expect("mixed outcomes should aggregate");

        let successful = outcomes.iter().filter(|o| o.result.passed).count() as u64;
        let total = outcomes.len() as u64;
        let all_pass = cases
            .iter()
            .filter(|case| {
                outcomes
                    .iter()
                    .filter(|o| o.case_id == case.case_id)
                    .all(|o| o.result.passed)
            })
            .count() as u64;
        assert!(
            successful > 0 && successful < total,
            "fixture must be mixed"
        );
        assert!(all_pass > 0 && all_pass < cases.len() as u64);

        let legacy_pass_at_1 = successful as f64 / total as f64;
        let legacy_pass_all_k = all_pass as f64 / cases.len() as f64;
        let legacy_variance = legacy_pass_at_1 * (1.0 - legacy_pass_at_1);
        assert_eq!(
            report.metrics.pass_at_1.expect("pass_at_1").to_bits(),
            legacy_pass_at_1.to_bits()
        );
        assert_eq!(
            report.metrics.pass_all_k.expect("pass_all_k").to_bits(),
            legacy_pass_all_k.to_bits()
        );
        assert_eq!(
            report.metrics.pass_variance.expect("variance").to_bits(),
            legacy_variance.to_bits()
        );
    }

    #[test]
    fn reliability_report_emits_the_full_curve_with_case_level_provenance_offline() {
        // Pins: the case is the resampling unit, the curve spans k=1..=5, and its
        // endpoints agree exactly with the two gated report metrics.
        let cases = corpus();
        let outcomes = mixed_outcomes(&cases);
        let report = aggregate(&cases, &outcomes).expect("mixed outcomes should aggregate");
        let reliability =
            live_execution_reliability(&cases, &outcomes, EXECUTION_LIVE_REPETITIONS, provider())
                .expect("complete repetitions should produce a reliability report");

        assert_eq!(reliability.resampling_unit, ReliabilityResamplingUnit::Case);
        assert_eq!(reliability.cases.len(), EXECUTION_LIVE_CASE_COUNT);
        assert!(
            reliability
                .cases
                .iter()
                .all(|case| case.attempts == EXECUTION_LIVE_REPETITIONS)
        );
        assert_eq!(reliability.curve.len(), EXECUTION_LIVE_REPETITIONS as usize);
        assert_eq!(
            reliability.at_k(1).map(|estimate| estimate.pass_any_at_k),
            report.metrics.pass_at_1
        );
        assert_eq!(
            reliability
                .at_k(EXECUTION_LIVE_REPETITIONS)
                .map(|estimate| estimate.pass_all_at_k),
            report.metrics.pass_all_k
        );
        assert_eq!(
            reliability.pooled_outcome_variance,
            report.metrics.pass_variance
        );
        assert!(reliability.integrity.is_complete());
        assert_eq!(reliability.integrity.counted_trials, 100);
        assert!(reliability.provenance.cost_complete);
        assert_eq!(
            reliability.provenance.seeds,
            cases.iter().map(|case| case.seed).collect::<Vec<_>>()
        );
        // The seed reproduces case selection only; the provider is still sampled,
        // which is exactly why five repetitions are recorded per case.
        assert!(
            reliability
                .curve
                .windows(2)
                .all(|pair| pair[0].pass_any_at_k <= pair[1].pass_any_at_k
                    && pair[0].pass_all_at_k >= pair[1].pass_all_at_k)
        );
    }

    #[test]
    fn duplicate_and_missing_repetitions_are_counted_not_silently_absorbed_offline() {
        // Pins: a duplicated run never inflates n, and the shortfall it creates is
        // reported and shortens the curve instead of being averaged away.
        let cases = corpus();
        let mut outcomes = mixed_outcomes(&cases);
        outcomes[3] = outcomes[2].clone();

        assert!(
            aggregate(&cases, &outcomes).is_err(),
            "the gated aggregate must still refuse a duplicate repetition"
        );

        let reliability =
            live_execution_reliability(&cases, &outcomes, EXECUTION_LIVE_REPETITIONS, provider())
                .expect("a duplicate should be reported, not lose the case");
        assert_eq!(reliability.integrity.duplicate_repetitions, 1);
        assert_eq!(reliability.integrity.missing_repetitions, 1);
        assert_eq!(reliability.integrity.counted_trials, 99);
        assert!(!reliability.integrity.is_complete());
        assert_eq!(reliability.cases[0].attempts, 4);
        assert_eq!(
            reliability.curve.len(),
            EXECUTION_LIVE_REPETITIONS as usize - 1,
            "k may not exceed the repetitions a case actually recorded"
        );
    }

    #[test]
    fn a_case_with_no_usable_repetition_is_refused_offline() {
        // Pins: dropping a whole case would quietly shrink the resampling unit.
        let cases = corpus();
        let outcomes = mixed_outcomes(&cases)
            .into_iter()
            .filter(|outcome| outcome.case_id != "live-000")
            .collect::<Vec<_>>();
        let error =
            live_execution_reliability(&cases, &outcomes, EXECUTION_LIVE_REPETITIONS, provider())
                .expect_err("a case with zero repetitions must be refused");
        assert!(error.to_string().contains("no usable repetition"));
    }
}
