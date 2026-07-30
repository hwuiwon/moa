//! Strict versioned reports for execution honesty and recovery evaluation.

use std::collections::{BTreeMap, BTreeSet};

use moa_core::types::execution_planning::{
    ExecutionRouteKind, ExecutionRouteProvenance, ExecutionStrategy,
};
use moa_eval_core::{Error, Result};
use moa_execution::state::ExecutionRunStatus;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    invariants::{ExecutionInvariantResult, ExecutionInvariantSpec, evaluate_invariants},
    snapshot::ExecutionEvalSnapshot,
};

/// Current schema version for execution-eval reports.
pub const EXECUTION_EVAL_REPORT_SCHEMA_VERSION: u8 = 1;
const MAX_CASE_ID_BYTES: usize = 256;
const MAX_INVARIANT_JSON_BYTES: usize = 16_384;
const MAX_DIAGNOSTIC_BYTES: usize = 1_024;
const FINAL_RESPONSE_HASH_DOMAIN: &[u8] = b"moa.execution-eval.final-response\0";

/// Execution-eval lane that produced a report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEvalLane {
    /// Pure functions and checked-in corpora used as a PR hard gate.
    OfflinePr,
    /// Scripted-provider production-path scenarios used as a PR hard gate.
    ServicePr,
    /// Full deterministic recovery and fault matrix.
    NightlyDeterministic,
    /// Repeated real-provider trend collection.
    NightlyLive,
    /// Targeted mutation-testing lane.
    Mutation,
}

/// Calibration state for any semantic judge metrics attached to a report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionJudgeCalibrationStatus {
    /// No complete human-label artifact was supplied; judged metrics are unavailable.
    Unavailable,
    /// The supplied artifact passed every agreement and judge-accuracy threshold.
    Calibrated,
    /// A supplied artifact failed at least one required threshold.
    Rejected,
}

/// Provider and model provenance for a live or recorded report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalProvider {
    /// Stable provider identifier.
    pub provider: String,
    /// Stable model identifier.
    pub model: String,
    /// Stable prompt or cassette version.
    pub prompt_version: String,
}

/// One case-level deterministic execution-eval result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalCaseResult {
    /// Stable corpus case identifier.
    pub case_id: String,
    /// Whether all case-level gates passed.
    pub passed: bool,
    /// Whether contract scoring found at least one user-requirement omission.
    pub contract_omission: Option<bool>,
    /// Deterministic contract macro-F1 when a gold contract is available.
    pub contract_score: Option<f64>,
    /// Whether observed evidence proves this case could not validly complete.
    pub impossible_case: bool,
    /// Whether an impossible case nevertheless reported completed.
    pub execution_false_completion: bool,
    /// Observed durable execution status, absent for non-execution routing cases.
    pub observed_run_status: Option<ExecutionRunStatus>,
    /// Final route kind for a routing-aware case.
    pub observed_route: Option<ExecutionRouteKind>,
    /// Deterministic strategy for an observed Execute route.
    pub observed_strategy: Option<ExecutionStrategy>,
    /// Exact redacted classifier provenance retained for a routing-aware case.
    pub route_provenance: Option<ExecutionRouteProvenance>,
    /// Ordered deterministic invariant results.
    pub invariants: Vec<ExecutionInvariantResult>,
    /// Reconciled provider and capability cost for the case.
    pub cost_microusd: u64,
    /// Measured case latency.
    pub latency_ms: u64,
    /// Number of persisted logical tasks.
    pub task_count: u64,
    /// Canonical terminal-output hash, never the output itself.
    pub terminal_output_hash: Option<String>,
    /// SHA-256 hash of the final synthesized response, never the response itself.
    pub final_response_hash: Option<String>,
}

impl ExecutionEvalCaseResult {
    /// Evaluates deterministic invariants and builds one redacted case result.
    pub fn evaluate(
        case_id: impl Into<String>,
        snapshot: &ExecutionEvalSnapshot,
        specs: &[ExecutionInvariantSpec],
        latency_ms: u64,
    ) -> Result<Self> {
        let invariants = evaluate_invariants(snapshot, specs);
        let impossible_case = invariants
            .iter()
            .any(|result| result.completion_guard_passed == Some(false));
        let execution_false_completion =
            impossible_case && snapshot.run.status == ExecutionRunStatus::Completed;
        let passed = invariants.iter().all(|result| result.passed) && !execution_false_completion;
        let task_count = u64::try_from(snapshot.tasks.len())
            .map_err(|_| invalid_config("execution case task count exceeds u64".to_string()))?;
        let final_response_hash = snapshot
            .harness
            .final_response
            .as_deref()
            .map(hash_final_response);
        Ok(Self {
            case_id: case_id.into(),
            passed,
            contract_omission: None,
            contract_score: None,
            impossible_case,
            execution_false_completion,
            observed_run_status: Some(snapshot.run.status),
            observed_route: None,
            observed_strategy: None,
            route_provenance: None,
            invariants,
            cost_microusd: snapshot.run.budget_ledger.consumed.cost_microusd,
            latency_ms,
            task_count,
            terminal_output_hash: snapshot.run.terminal_output_hash.clone(),
            final_response_hash,
        })
    }
}

/// Aggregate metrics and exact denominators for one execution-eval report.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalAggregateMetrics {
    /// Number of case rows.
    pub total_cases: u64,
    /// Number of passing case rows.
    pub passed_cases: u64,
    /// Number of cases with contract-scoring results.
    pub contract_cases: u64,
    /// Number of contract cases with at least one omission.
    pub contract_omissions: u64,
    /// Number of cases that could not validly complete.
    pub impossible_cases: u64,
    /// Number of impossible cases observed as completed.
    ///
    /// This is the authoritative false-success signal for the lane. It is derived
    /// only from typed completion guards and durable run state, never from wording
    /// in a generated response, so it stays valid regardless of how a run phrased
    /// its answer.
    pub execution_false_completions: u64,
    /// Contract omissions divided by contract cases.
    pub contract_omission_rate: Option<f64>,
    /// Mean deterministic contract macro-F1 over scored cases.
    pub contract_macro_f1: Option<f64>,
    /// False completions divided by impossible cases.
    pub execution_false_completion_rate: Option<f64>,
    /// Asymmetric routing cost, when a routing corpus was evaluated.
    pub weighted_routing_cost: Option<f64>,
    /// Asymmetric Inline/Durable strategy cost, when Execute cases were evaluated.
    pub weighted_strategy_cost: Option<f64>,
    /// True Execute cases incorrectly predicted Respond.
    pub respond_on_execute_rate: Option<f64>,
    /// Recall over adjudicated near-boundary Execute/Inline cases.
    pub near_boundary_inline_recall: Option<f64>,
    /// Recall over all expected Durable strategies.
    pub durable_strategy_recall: Option<f64>,
    /// Recall over typed Inline-to-Durable upgrade cases.
    pub durable_upgrade_recall: Option<f64>,
    /// Exact evidence preservation across typed Durable-upgrade cases.
    pub durable_upgrade_evidence_preservation_rate: Option<f64>,
    /// True NeedsInput cases incorrectly accepted into a concrete mode.
    pub needs_input_false_accept_rate: Option<f64>,
    /// True concrete-mode cases unnecessarily predicted NeedsInput.
    pub unnecessary_clarification_rate: Option<f64>,
    /// Non-accepted classifier routes divided by classifier-attempted routes.
    pub classifier_fallback_rate: Option<f64>,
    /// Exact non-accepted classifier outcomes by closed outcome label.
    pub classifier_fallback_counts: Option<BTreeMap<String, u64>>,
    /// Mean classifier tokens per classifier-attempted route.
    pub classifier_tokens_per_routed_turn: Option<f64>,
    /// Mean classifier cost per classifier-attempted route.
    pub classifier_cost_microusd_per_routed_turn: Option<f64>,
    /// Mean classifier latency in milliseconds per classifier-attempted route.
    pub classifier_latency_ms_per_routed_turn: Option<f64>,
    /// Mean over logical cases of `moa_eval_core::reliability::pass_any_at_k` at `k = 1`.
    ///
    /// Computed per logical case and only then averaged; successes are never pooled
    /// across cases. At `k = 1` the combinatorial estimator collapses to each case's
    /// observed success rate.
    pub pass_at_1: Option<f64>,
    /// Mean over logical cases of `moa_eval_core::reliability::pass_all_at_k` at `k = repetitions`.
    ///
    /// At `k = n` this is the fraction of cases whose every repetition passed. Only
    /// independent repetitions may feed it; shared-prefix or branched rollouts are
    /// refused by the shared estimator.
    pub pass_all_k: Option<f64>,
    /// Population variance `p * (1 - p)` of the pooled repetition outcomes.
    ///
    /// This deliberately pools individual repetition rows to describe outcome spread.
    /// It is not the uncertainty of [`Self::pass_at_1`] across cases; the case-level
    /// view is the reliability curve emitted beside this report.
    pub pass_variance: Option<f64>,
    /// Cost per successful case in micro-US-dollars.
    pub cost_per_success_microusd: Option<f64>,
    /// Task-count ratio against a reference plan.
    pub task_count_ratio_vs_reference: Option<f64>,
    /// Caught viable mutants divided by all viable mutants.
    pub mutation_score: Option<f64>,
}

impl ExecutionEvalAggregateMetrics {
    /// Computes exact count-derived metrics from case rows.
    pub fn from_cases(cases: &[ExecutionEvalCaseResult]) -> Result<Self> {
        let total_cases = usize_to_u64(cases.len(), "total execution eval cases")?;
        let passed_cases = usize_to_u64(
            cases.iter().filter(|case| case.passed).count(),
            "passing execution eval cases",
        )?;
        let contract_cases = usize_to_u64(
            cases
                .iter()
                .filter(|case| case.contract_omission.is_some())
                .count(),
            "execution contract cases",
        )?;
        let contract_omissions = usize_to_u64(
            cases
                .iter()
                .filter(|case| case.contract_omission == Some(true))
                .count(),
            "execution contract omissions",
        )?;
        let impossible_cases = usize_to_u64(
            cases.iter().filter(|case| case.impossible_case).count(),
            "impossible execution cases",
        )?;
        let execution_false_completions = usize_to_u64(
            cases
                .iter()
                .filter(|case| case.execution_false_completion)
                .count(),
            "execution false completions",
        )?;
        Ok(Self {
            total_cases,
            passed_cases,
            contract_cases,
            contract_omissions,
            impossible_cases,
            execution_false_completions,
            contract_omission_rate: ratio(contract_omissions, contract_cases),
            contract_macro_f1: mean_optional(cases.iter().map(|case| case.contract_score)),
            execution_false_completion_rate: ratio(execution_false_completions, impossible_cases),
            weighted_routing_cost: None,
            weighted_strategy_cost: None,
            respond_on_execute_rate: None,
            near_boundary_inline_recall: None,
            durable_strategy_recall: None,
            durable_upgrade_recall: None,
            durable_upgrade_evidence_preservation_rate: None,
            needs_input_false_accept_rate: None,
            unnecessary_clarification_rate: None,
            classifier_fallback_rate: None,
            classifier_fallback_counts: None,
            classifier_tokens_per_routed_turn: None,
            classifier_cost_microusd_per_routed_turn: None,
            classifier_latency_ms_per_routed_turn: None,
            pass_at_1: None,
            pass_all_k: None,
            pass_variance: None,
            cost_per_success_microusd: None,
            task_count_ratio_vs_reference: None,
            mutation_score: None,
        })
    }
}

/// Strict, versioned execution evaluation report.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalReport {
    /// Report schema version, fixed at `1`.
    pub schema_version: u8,
    /// Lane that produced this report.
    pub lane: ExecutionEvalLane,
    /// Exact lowercase SHA-256 hashes for every checked-in or generated corpus.
    pub corpus_hashes: BTreeMap<String, String>,
    /// Corpus and bootstrap seeds, recorded as provenance only.
    ///
    /// A seed fixes case selection, ordering, and resampling so a report can be
    /// reproduced. It does not make a sampled provider deterministic, which is why
    /// the lane records several independent repetitions per case.
    pub seeds: Vec<u64>,
    /// Number of independent outcomes recorded per logical case.
    ///
    /// Independence is what licenses the combinatorial reliability estimators;
    /// repetitions that branch from a shared prefix do not qualify.
    pub repetitions: u32,
    /// Calibration status governing any semantic judge metrics.
    pub judge_calibration_status: ExecutionJudgeCalibrationStatus,
    /// Provider/model provenance when applicable.
    pub provider: Option<ExecutionEvalProvider>,
    /// Stable per-case results.
    pub cases: Vec<ExecutionEvalCaseResult>,
    /// Exact report aggregates.
    pub metrics: ExecutionEvalAggregateMetrics,
}

impl ExecutionEvalReport {
    /// Builds and validates a report from strict case rows.
    pub fn new(
        lane: ExecutionEvalLane,
        corpus_hashes: BTreeMap<String, String>,
        seeds: Vec<u64>,
        repetitions: u32,
        judge_calibration_status: ExecutionJudgeCalibrationStatus,
        provider: Option<ExecutionEvalProvider>,
        cases: Vec<ExecutionEvalCaseResult>,
    ) -> Result<Self> {
        let metrics = ExecutionEvalAggregateMetrics::from_cases(&cases)?;
        let report = Self {
            schema_version: EXECUTION_EVAL_REPORT_SCHEMA_VERSION,
            lane,
            corpus_hashes,
            seeds,
            repetitions,
            judge_calibration_status,
            provider,
            cases,
            metrics,
        };
        report.validate()?;
        Ok(report)
    }

    /// Validates versions, identities, hashes, redaction bounds, and arithmetic.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != EXECUTION_EVAL_REPORT_SCHEMA_VERSION {
            return Err(invalid_config(format!(
                "execution eval report version {} is unsupported; expected {EXECUTION_EVAL_REPORT_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        validate_corpus_hashes(&self.corpus_hashes)?;
        if self.repetitions == 0 {
            return Err(invalid_config(
                "execution eval report repetitions must be positive".to_string(),
            ));
        }
        validate_provider(self.provider.as_ref())?;

        let mut case_ids = BTreeSet::new();
        for case in &self.cases {
            validate_case(case)?;
            if !case_ids.insert(case.case_id.as_str()) {
                return Err(invalid_config(format!(
                    "duplicate execution eval case ID `{}`",
                    case.case_id
                )));
            }
        }

        validate_metrics(&self.metrics)?;
        let computed = ExecutionEvalAggregateMetrics::from_cases(&self.cases)?;
        validate_count_derived_metrics(&self.metrics, &computed)
    }

    /// Serializes a validated report as pretty JSON.
    pub fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }
}

fn validate_case(case: &ExecutionEvalCaseResult) -> Result<()> {
    if case.case_id.trim().is_empty() || case.case_id.len() > MAX_CASE_ID_BYTES {
        return Err(invalid_config(format!(
            "execution eval case ID must contain 1..={MAX_CASE_ID_BYTES} bytes"
        )));
    }
    validate_optional_hash("terminal_output_hash", case.terminal_output_hash.as_deref())?;
    validate_optional_hash("final_response_hash", case.final_response_hash.as_deref())?;
    validate_optional_rate("contract_score", case.contract_score)?;
    if !matches!(
        (case.observed_route, case.observed_strategy),
        (None, None)
            | (
                Some(ExecutionRouteKind::Respond | ExecutionRouteKind::NeedsInput),
                None
            )
            | (
                Some(ExecutionRouteKind::Execute),
                Some(ExecutionStrategy::Inline | ExecutionStrategy::Durable)
            )
    ) {
        return Err(invalid_config(format!(
            "execution eval case `{}` has an inconsistent route and strategy",
            case.case_id
        )));
    }

    let mut invariant_ids = BTreeSet::new();
    for invariant in &case.invariants {
        if invariant.invariant_id.trim().is_empty()
            || !invariant_ids.insert(invariant.invariant_id.as_str())
        {
            return Err(invalid_config(format!(
                "execution eval case `{}` has an empty or duplicate invariant ID `{}`",
                case.case_id, invariant.invariant_id
            )));
        }
        if invariant.diagnostic.len() > MAX_DIAGNOSTIC_BYTES
            || serde_json::to_vec(&invariant.expected)?.len() > MAX_INVARIANT_JSON_BYTES
            || serde_json::to_vec(&invariant.observed)?.len() > MAX_INVARIANT_JSON_BYTES
        {
            return Err(invalid_config(format!(
                "execution eval case `{}` has an oversized invariant result",
                case.case_id
            )));
        }
    }

    let impossible_case = case
        .invariants
        .iter()
        .any(|result| result.completion_guard_passed == Some(false));
    if case.impossible_case != impossible_case {
        return Err(invalid_config(format!(
            "execution eval case `{}` has inconsistent impossible-case arithmetic",
            case.case_id
        )));
    }
    let false_completion =
        impossible_case && case.observed_run_status == Some(ExecutionRunStatus::Completed);
    if case.execution_false_completion != false_completion {
        return Err(invalid_config(format!(
            "execution eval case `{}` has inconsistent false-completion arithmetic",
            case.case_id
        )));
    }
    if case.passed
        && (case.execution_false_completion
            || case.contract_omission == Some(true)
            || case.invariants.iter().any(|result| !result.passed))
    {
        return Err(invalid_config(format!(
            "execution eval case `{}` reports pass despite a hard failure",
            case.case_id
        )));
    }
    Ok(())
}

fn validate_corpus_hashes(hashes: &BTreeMap<String, String>) -> Result<()> {
    if hashes.is_empty() {
        return Err(invalid_config(
            "execution eval report requires at least one corpus hash".to_string(),
        ));
    }
    for (name, hash) in hashes {
        if name.trim().is_empty() {
            return Err(invalid_config(
                "execution eval corpus hash name cannot be empty".to_string(),
            ));
        }
        validate_hash(name, hash)?;
    }
    Ok(())
}

fn validate_provider(provider: Option<&ExecutionEvalProvider>) -> Result<()> {
    let Some(provider) = provider else {
        return Ok(());
    };
    if provider.provider.trim().is_empty()
        || provider.model.trim().is_empty()
        || provider.prompt_version.trim().is_empty()
    {
        return Err(invalid_config(
            "execution eval provider provenance fields cannot be empty".to_string(),
        ));
    }
    Ok(())
}

fn validate_metrics(metrics: &ExecutionEvalAggregateMetrics) -> Result<()> {
    validate_optional_rate("contract_omission_rate", metrics.contract_omission_rate)?;
    validate_optional_rate("contract_macro_f1", metrics.contract_macro_f1)?;
    validate_optional_rate(
        "execution_false_completion_rate",
        metrics.execution_false_completion_rate,
    )?;
    validate_optional_nonnegative("weighted_routing_cost", metrics.weighted_routing_cost)?;
    validate_optional_nonnegative("weighted_strategy_cost", metrics.weighted_strategy_cost)?;
    validate_optional_rate("respond_on_execute_rate", metrics.respond_on_execute_rate)?;
    validate_optional_rate(
        "near_boundary_inline_recall",
        metrics.near_boundary_inline_recall,
    )?;
    validate_optional_rate("durable_strategy_recall", metrics.durable_strategy_recall)?;
    validate_optional_rate("durable_upgrade_recall", metrics.durable_upgrade_recall)?;
    validate_optional_rate(
        "durable_upgrade_evidence_preservation_rate",
        metrics.durable_upgrade_evidence_preservation_rate,
    )?;
    validate_optional_rate(
        "needs_input_false_accept_rate",
        metrics.needs_input_false_accept_rate,
    )?;
    validate_optional_rate(
        "unnecessary_clarification_rate",
        metrics.unnecessary_clarification_rate,
    )?;
    validate_optional_rate("classifier_fallback_rate", metrics.classifier_fallback_rate)?;
    validate_classifier_fallback_counts(metrics.classifier_fallback_counts.as_ref())?;
    validate_optional_nonnegative(
        "classifier_tokens_per_routed_turn",
        metrics.classifier_tokens_per_routed_turn,
    )?;
    validate_optional_nonnegative(
        "classifier_cost_microusd_per_routed_turn",
        metrics.classifier_cost_microusd_per_routed_turn,
    )?;
    validate_optional_nonnegative(
        "classifier_latency_ms_per_routed_turn",
        metrics.classifier_latency_ms_per_routed_turn,
    )?;
    validate_optional_rate("pass_at_1", metrics.pass_at_1)?;
    validate_optional_rate("pass_all_k", metrics.pass_all_k)?;
    validate_optional_nonnegative("pass_variance", metrics.pass_variance)?;
    validate_optional_nonnegative(
        "cost_per_success_microusd",
        metrics.cost_per_success_microusd,
    )?;
    validate_optional_nonnegative(
        "task_count_ratio_vs_reference",
        metrics.task_count_ratio_vs_reference,
    )?;
    validate_optional_rate("mutation_score", metrics.mutation_score)
}

fn validate_classifier_fallback_counts(counts: Option<&BTreeMap<String, u64>>) -> Result<()> {
    let Some(counts) = counts else {
        return Ok(());
    };
    let allowed = [
        "provider_error",
        "stream_error",
        "oversized",
        "schema_rejected",
        "invalid_decision",
        "low_confidence",
        "context_forced_inline",
    ];
    if counts
        .iter()
        .any(|(outcome, count)| !allowed.contains(&outcome.as_str()) || *count == 0)
    {
        return Err(invalid_config(
            "classifier fallback counts contain an unknown outcome or zero count".to_string(),
        ));
    }
    Ok(())
}

fn validate_count_derived_metrics(
    actual: &ExecutionEvalAggregateMetrics,
    expected: &ExecutionEvalAggregateMetrics,
) -> Result<()> {
    if actual.total_cases != expected.total_cases
        || actual.passed_cases != expected.passed_cases
        || actual.contract_cases != expected.contract_cases
        || actual.contract_omissions != expected.contract_omissions
        || actual.impossible_cases != expected.impossible_cases
        || actual.execution_false_completions != expected.execution_false_completions
        || !same_optional_float(
            actual.contract_omission_rate,
            expected.contract_omission_rate,
        )
        || !same_optional_float(actual.contract_macro_f1, expected.contract_macro_f1)
        || !same_optional_float(
            actual.execution_false_completion_rate,
            expected.execution_false_completion_rate,
        )
    {
        return Err(invalid_config(
            "execution eval aggregate counts or count-derived rates disagree with case rows"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_optional_rate(name: &str, value: Option<f64>) -> Result<()> {
    if value.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
        return Err(invalid_config(format!(
            "execution eval metric `{name}` must be finite and within [0, 1]"
        )));
    }
    Ok(())
}

fn validate_optional_nonnegative(name: &str, value: Option<f64>) -> Result<()> {
    if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return Err(invalid_config(format!(
            "execution eval metric `{name}` must be finite and nonnegative"
        )));
    }
    Ok(())
}

fn validate_optional_hash(name: &str, value: Option<&str>) -> Result<()> {
    value.map_or(Ok(()), |value| validate_hash(name, value))
}

fn validate_hash(name: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid_config(format!(
            "execution eval hash `{name}` must be 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn hash_final_response(response: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FINAL_RESPONSE_HASH_DOMAIN);
    hasher.update(response.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator != 0).then_some(numerator as f64 / denominator as f64)
}

fn mean_optional(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let values = values.flatten().collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn same_optional_float(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() <= f64::EPSILON,
        (None, None) => true,
        _ => false,
    }
}

fn usize_to_u64(value: usize, context: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| invalid_config(format!("{context} exceeds the report integer range")))
}

fn invalid_config(message: String) -> Error {
    Error::InvalidConfig(message)
}
