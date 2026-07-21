//! Strict paired comparison and cargo-mutants reporting for execution evaluation.

use std::collections::{BTreeMap, BTreeSet};

use moa_eval_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kernel::{
    BinaryProbeOutcome, BootstrapConfig, ClusterBootstrapReport, ClusterObservation,
    PairedComparison, benjamini_hochberg, cluster_bootstrap_mean_by_user, mcnemar_paired_test,
};

use super::report::{ExecutionEvalCaseResult, ExecutionEvalLane, ExecutionEvalReport};

/// Current schema version for paired execution-eval comparisons.
pub const EXECUTION_EVAL_COMPARISON_SCHEMA_VERSION: u8 = 1;
/// Current schema version for targeted execution mutation reports.
pub const EXECUTION_MUTATION_REPORT_SCHEMA_VERSION: u8 = 1;

/// Statistical and practical thresholds for one paired report comparison.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalComparisonConfig {
    /// Cluster-bootstrap configuration used for every paired numeric delta.
    pub bootstrap: BootstrapConfig,
    /// False-discovery rate applied to paired binary tests.
    pub false_discovery_rate: f64,
    /// Minimum candidate pass-rate decrease treated as practically meaningful.
    pub practical_pass_rate_regression: f64,
}

impl Default for ExecutionEvalComparisonConfig {
    fn default() -> Self {
        Self {
            bootstrap: BootstrapConfig::default(),
            false_discovery_rate: 0.05,
            practical_pass_rate_regression: 0.02,
        }
    }
}

/// Strict paired comparison over execution-eval case IDs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvalComparison {
    /// Comparison schema version, fixed at `1`.
    pub schema_version: u8,
    /// Number of exactly paired case IDs.
    pub paired_cases: u64,
    /// BH-corrected paired pass/fail comparison.
    pub pass_comparison: PairedComparison,
    /// Candidate-minus-baseline pass-rate delta and interval.
    pub pass_rate_delta: ClusterBootstrapReport,
    /// Candidate-minus-baseline cost delta and interval.
    pub cost_microusd_delta: ClusterBootstrapReport,
    /// Candidate-minus-baseline latency delta and interval.
    pub latency_ms_delta: ClusterBootstrapReport,
    /// Candidate-minus-baseline task-count delta and interval.
    pub task_count_delta: ClusterBootstrapReport,
    /// Configured minimum practically meaningful pass-rate regression.
    pub practical_pass_rate_regression: f64,
    /// Whether the pass regression is both significant and practically meaningful.
    pub significant_pass_regression: bool,
    /// Whether a live-to-live comparison should fail its trend gate.
    pub gate_failed: bool,
}

/// Strict summary of cargo-mutants outcomes for the targeted execution lane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMutationReport {
    /// Mutation report schema version, fixed at `1`.
    pub schema_version: u8,
    /// Mutants caught by at least one selected test.
    pub caught: u64,
    /// Viable mutants that survived the selected tests.
    pub missed: u64,
    /// Viable mutants that timed out and therefore do not count as caught.
    pub timeouts: u64,
    /// Mutants that could not compile or run and are excluded from the denominator.
    pub unviable: u64,
    /// Caught, missed, and timeout mutants in the score denominator.
    pub viable: u64,
    /// Caught divided by viable mutants.
    pub mutation_score: f64,
    /// Stable descriptions of missed mutants retained for triage.
    pub missed_mutants: Vec<String>,
    /// Stable descriptions of timed-out mutants retained for triage.
    pub timeout_mutants: Vec<String>,
}

/// Compares two validated reports after enforcing exact paired identity.
pub fn compare_execution_eval_reports(
    baseline: &ExecutionEvalReport,
    candidate: &ExecutionEvalReport,
    config: ExecutionEvalComparisonConfig,
) -> Result<ExecutionEvalComparison> {
    baseline.validate()?;
    candidate.validate()?;
    validate_comparison_config(config)?;
    validate_pairing(baseline, candidate)?;

    let baseline_by_case = case_map(&baseline.cases);
    let candidate_by_case = case_map(&candidate.cases);
    let pass_comparison = benjamini_hochberg(
        vec![mcnemar_paired_test(
            "case_passed",
            &binary_outcomes(&baseline.cases),
            &binary_outcomes(&candidate.cases),
        )],
        config.false_discovery_rate,
    )
    .into_iter()
    .next()
    .ok_or_else(|| invalid_config("paired pass comparison disappeared".to_string()))?;
    let pass_rate_delta = paired_interval(
        "pass_rate_delta",
        &baseline_by_case,
        &candidate_by_case,
        config.bootstrap,
        |case| if case.passed { 1.0 } else { 0.0 },
    );
    let cost_microusd_delta = paired_interval(
        "cost_microusd_delta",
        &baseline_by_case,
        &candidate_by_case,
        config.bootstrap,
        |case| case.cost_microusd as f64,
    );
    let latency_ms_delta = paired_interval(
        "latency_ms_delta",
        &baseline_by_case,
        &candidate_by_case,
        config.bootstrap,
        |case| case.latency_ms as f64,
    );
    let task_count_delta = paired_interval(
        "task_count_delta",
        &baseline_by_case,
        &candidate_by_case,
        config.bootstrap,
        |case| case.task_count as f64,
    );
    let significant_pass_regression = pass_comparison.significant
        && pass_rate_delta.mean < -config.practical_pass_rate_regression;
    let live_comparison = baseline.lane == ExecutionEvalLane::NightlyLive
        && candidate.lane == ExecutionEvalLane::NightlyLive;

    Ok(ExecutionEvalComparison {
        schema_version: EXECUTION_EVAL_COMPARISON_SCHEMA_VERSION,
        paired_cases: u64::try_from(baseline.cases.len())
            .map_err(|_| invalid_config("paired case count exceeds u64".to_string()))?,
        pass_comparison,
        pass_rate_delta,
        cost_microusd_delta,
        latency_ms_delta,
        task_count_delta,
        practical_pass_rate_regression: config.practical_pass_rate_regression,
        significant_pass_regression,
        gate_failed: live_comparison && significant_pass_regression,
    })
}

/// Parses cargo-mutants `outcomes.json` while preserving every missed and timeout identity.
pub fn mutation_report_from_outcomes(value: &Value) -> Result<ExecutionMutationReport> {
    let outcomes = value
        .as_array()
        .or_else(|| value.get("outcomes").and_then(Value::as_array))
        .ok_or_else(|| {
            invalid_config("cargo-mutants outcomes must contain an array".to_string())
        })?;
    let mut caught = 0_u64;
    let mut missed = 0_u64;
    let mut timeouts = 0_u64;
    let mut unviable = 0_u64;
    let mut missed_mutants = Vec::new();
    let mut timeout_mutants = Vec::new();

    for (index, outcome) in outcomes.iter().enumerate() {
        let Some(status) = outcome_status(outcome) else {
            continue;
        };
        let name = outcome_name(outcome, index);
        match status {
            MutationOutcomeClass::Caught => caught = caught.saturating_add(1),
            MutationOutcomeClass::Missed => {
                missed = missed.saturating_add(1);
                missed_mutants.push(name);
            }
            MutationOutcomeClass::Timeout => {
                timeouts = timeouts.saturating_add(1);
                timeout_mutants.push(name);
            }
            MutationOutcomeClass::Unviable => unviable = unviable.saturating_add(1),
        }
    }
    let viable = caught.saturating_add(missed).saturating_add(timeouts);
    if viable == 0 {
        return Err(invalid_config(
            "cargo-mutants outcomes contain no viable selected mutants".to_string(),
        ));
    }
    missed_mutants.sort();
    timeout_mutants.sort();
    Ok(ExecutionMutationReport {
        schema_version: EXECUTION_MUTATION_REPORT_SCHEMA_VERSION,
        caught,
        missed,
        timeouts,
        unviable,
        viable,
        mutation_score: caught as f64 / viable as f64,
        missed_mutants,
        timeout_mutants,
    })
}

fn validate_comparison_config(config: ExecutionEvalComparisonConfig) -> Result<()> {
    if config.bootstrap.resamples == 0
        || !config.false_discovery_rate.is_finite()
        || !(0.0..=1.0).contains(&config.false_discovery_rate)
        || !config.practical_pass_rate_regression.is_finite()
        || !(0.0..=1.0).contains(&config.practical_pass_rate_regression)
    {
        return Err(invalid_config(
            "execution comparison thresholds and bootstrap resamples are invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_pairing(baseline: &ExecutionEvalReport, candidate: &ExecutionEvalReport) -> Result<()> {
    if baseline.corpus_hashes != candidate.corpus_hashes {
        return Err(invalid_config(
            "execution comparison refused different corpus hashes".to_string(),
        ));
    }
    if baseline.seeds != candidate.seeds {
        return Err(invalid_config(
            "execution comparison refused different seed sets".to_string(),
        ));
    }
    if baseline.repetitions != candidate.repetitions {
        return Err(invalid_config(
            "execution comparison refused different repetition counts".to_string(),
        ));
    }
    let baseline_ids = case_ids(&baseline.cases);
    let candidate_ids = case_ids(&candidate.cases);
    if baseline_ids != candidate_ids {
        return Err(invalid_config(
            "execution comparison refused different case ID sets".to_string(),
        ));
    }
    Ok(())
}

fn case_ids(cases: &[ExecutionEvalCaseResult]) -> BTreeSet<&str> {
    cases.iter().map(|case| case.case_id.as_str()).collect()
}

fn case_map(cases: &[ExecutionEvalCaseResult]) -> BTreeMap<&str, &ExecutionEvalCaseResult> {
    cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect()
}

fn binary_outcomes(cases: &[ExecutionEvalCaseResult]) -> Vec<BinaryProbeOutcome> {
    cases
        .iter()
        .map(|case| BinaryProbeOutcome {
            probe_id: case.case_id.clone(),
            success: case.passed,
        })
        .collect()
}

fn paired_interval(
    metric_name: &str,
    baseline: &BTreeMap<&str, &ExecutionEvalCaseResult>,
    candidate: &BTreeMap<&str, &ExecutionEvalCaseResult>,
    config: BootstrapConfig,
    metric: impl Fn(&ExecutionEvalCaseResult) -> f64,
) -> ClusterBootstrapReport {
    let observations = baseline
        .iter()
        .filter_map(|(case_id, baseline_case)| {
            candidate
                .get(case_id)
                .map(|candidate_case| ClusterObservation {
                    user_id: logical_case_id(case_id).to_string(),
                    probe_id: (*case_id).to_string(),
                    value: metric(candidate_case) - metric(baseline_case),
                })
        })
        .collect::<Vec<_>>();
    cluster_bootstrap_mean_by_user(metric_name, &observations, config)
}

fn logical_case_id(case_id: &str) -> &str {
    case_id
        .split_once("#run=")
        .map_or(case_id, |(base, _)| base)
}

#[derive(Clone, Copy)]
enum MutationOutcomeClass {
    Caught,
    Missed,
    Timeout,
    Unviable,
}

fn outcome_status(outcome: &Value) -> Option<MutationOutcomeClass> {
    let status = ["summary", "outcome", "status"]
        .iter()
        .find_map(|key| outcome.get(key).and_then(Value::as_str))?
        .to_ascii_lowercase();
    if status.contains("caught") || status.contains("killed") {
        Some(MutationOutcomeClass::Caught)
    } else if status.contains("missed") || status.contains("survived") {
        Some(MutationOutcomeClass::Missed)
    } else if status.contains("timeout") {
        Some(MutationOutcomeClass::Timeout)
    } else if status.contains("unviable")
        || status.contains("buildfailure")
        || status.contains("build_failure")
    {
        Some(MutationOutcomeClass::Unviable)
    } else {
        None
    }
}

fn outcome_name(outcome: &Value, index: usize) -> String {
    for key in ["mutant", "scenario", "name"] {
        if let Some(value) = outcome.get(key) {
            if let Some(name) = value.as_str() {
                return name.to_string();
            }
            if let Ok(name) = serde_json::to_string(value) {
                return name;
            }
        }
    }
    format!("mutant-{index}")
}

fn invalid_config(message: String) -> Error {
    Error::InvalidConfig(message)
}
