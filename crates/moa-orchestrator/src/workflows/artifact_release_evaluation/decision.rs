//! Deterministic release decisions derived from production experiment evidence.
//!
//! This module does not run another evaluator. It reads the exact trial rows and
//! score provenance produced by `ExperimentTrialRun`, pairs the candidate and
//! baseline variants on their plan coordinates, applies the predeclared release
//! policy with MOA's existing paired non-inferiority machinery, and returns the
//! evidence identifiers the release transaction must attest. Until the exact
//! production design has an operating-characteristic assessment, comparative
//! statistics are diagnostic; activation authority comes only from the
//! candidate's absolute deterministic evidence.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::registry::ReleaseRepository;
use moa_artifacts::release::{
    AssertionRef, DeterministicVerdict, Digest32, GateConfidenceMethod, GateMetric, GateMetricUnit,
    MetricDirection, TenantScope,
};
use moa_core::types::experiments::{ScorecardEligibility, ScorecardValueType};
use moa_core::types::identifiers::TenantId;
use moa_eval::kernel::stats::{
    PairedBinaryObservation, PairedNumericObservation, evaluate_paired_binary_gate,
    evaluate_paired_numeric_gate,
};
use moa_eval_core::decision::{
    Decision, GateOutcome, RegressionDeclaration, holm_regression_family, intersection_union_gate,
};
use moa_eval_core::metric::{
    ConfidenceMethod, Estimand, Estimator, GateKind, HypothesisFamily, MetricClass,
    MetricDefinition, MetricDirection as EvalMetricDirection, MetricUnit, ResamplingPlan,
};
use moa_experiments::model::{ExperimentRunStatus, ExperimentTrialStatus};
use moa_experiments::store::ExperimentStore;
use moa_scoring::{ExperimentRunScoreRowsRef, exact_experiment_run_score_rows_for_tenant};
use moa_wire::experiments::{
    ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
    ExperimentScoresRequest, ExperimentTrialScoreSummary,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::Error;
use super::types::ProvisionedTrial;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReleaseTrialIdentity {
    trial_key: String,
    variant_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReleaseTrialCompletion {
    identity: ReleaseTrialIdentity,
    status: ExperimentTrialStatus,
}

fn require_completed_release_trial_set(
    run_status: ExperimentRunStatus,
    declared_expected_trials: u64,
    expected: &[ReleaseTrialIdentity],
    observed: &[ReleaseTrialCompletion],
) -> Result<(), Error> {
    if expected.is_empty() {
        return Err(Error::ExperimentBindingInvalid(
            "release experiment has no provisioned candidate trials".to_string(),
        ));
    }
    if run_status != ExperimentRunStatus::Completed {
        return Err(Error::ExperimentBindingInvalid(format!(
            "release experiment is {} rather than completed",
            run_status.as_str()
        )));
    }
    let expected_count = u64::try_from(expected.len()).map_err(|_| {
        Error::ExperimentBindingInvalid(
            "provisioned release trial count exceeds the supported range".to_string(),
        )
    })?;
    if declared_expected_trials != expected_count {
        return Err(Error::ExperimentBindingInvalid(format!(
            "release experiment declared {declared_expected_trials} trials but provisioned {expected_count}"
        )));
    }
    if observed.len() != expected.len() {
        return Err(Error::ExperimentBindingInvalid(format!(
            "release experiment completed with {} of {} provisioned trials",
            observed.len(),
            expected.len()
        )));
    }
    if observed
        .iter()
        .any(|trial| trial.status != ExperimentTrialStatus::Completed)
    {
        return Err(Error::ExperimentBindingInvalid(
            "release experiment contains a non-completed provisioned trial".to_string(),
        ));
    }
    let expected_len = expected.len();
    let expected = expected.iter().collect::<BTreeSet<_>>();
    let observed = observed
        .iter()
        .map(|trial| &trial.identity)
        .collect::<BTreeSet<_>>();
    if expected.len() != expected_len || observed.len() != expected_len || observed != expected {
        return Err(Error::ExperimentBindingInvalid(
            "release experiment does not contain every provisioned trial key and arm exactly once"
                .to_string(),
        ));
    }
    Ok(())
}

/// Deterministic decision and exact evidence consumed by release settlement.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExperimentReleaseDecision {
    /// Three-way release verdict.
    pub verdict: DeterministicVerdict,
    /// Trials whose persisted evidence was considered.
    pub trial_uids: Vec<Uuid>,
    /// Score rows whose provenance was considered.
    pub evidence_ids: Vec<Uuid>,
    /// Stable per-assertion and per-metric outcomes.
    pub gate_results: BTreeMap<String, String>,
    /// Exact deterministic assertions required by the resolved release policy.
    pub blocking_assertions: Vec<AssertionRef>,
    /// Full deterministic decision detail stored on the release-attempt row.
    pub detail: Value,
}

/// Derives a release verdict from one completed production experiment run.
pub async fn decide_completed_run(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    run_uid: Uuid,
    candidate_revision_uid: Uuid,
    subject_digest: Digest32,
    provisioned_trials: &[ProvisionedTrial],
) -> Result<ExperimentReleaseDecision, Error> {
    let scope = TenantScope::new(tenant_id);
    let release_repository = ReleaseRepository::new(pool.clone());
    let candidate = release_repository
        .load_candidate(&scope, candidate_revision_uid)
        .await?
        .ok_or_else(|| {
            Error::StaleDispatch(format!(
                "release candidate {candidate_revision_uid} disappeared before settlement"
            ))
        })?;
    if candidate.subject_digest != subject_digest {
        return Err(Error::StaleDispatch(format!(
            "release candidate {candidate_revision_uid} subject changed before settlement"
        )));
    }
    let policy = release_repository
        .resolve_policy(&scope, candidate.activation_target.class())
        .await?;
    if policy.identity() != candidate.policy {
        return Err(Error::StaleDispatch(format!(
            "release policy changed while candidate {candidate_revision_uid} was running"
        )));
    }

    let experiment_store = ExperimentStore::new(pool.clone());
    let run = experiment_store
        .load_run_for_workflow(tenant_id, run_uid)
        .await
        .map_err(|error| Error::Storage(error.to_string()))?
        .ok_or_else(|| {
            Error::ExperimentBindingInvalid(format!(
                "release experiment run {run_uid} does not exist"
            ))
        })?;
    let trials = experiment_store
        .list_trials(&run.scope, run_uid, None, i64::MAX)
        .await
        .map_err(|error| Error::Storage(error.to_string()))?;
    let expected = provisioned_trials
        .iter()
        .map(|trial| ReleaseTrialIdentity {
            trial_key: trial.trial_key.clone(),
            variant_key: match trial.role {
                super::types::ArmRole::Candidate => ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
                super::types::ArmRole::Baseline => ARTIFACT_RELEASE_BASELINE_VARIANT_KEY,
            }
            .to_string(),
        })
        .collect::<Vec<_>>();
    let observed = trials
        .iter()
        .map(|trial| ReleaseTrialCompletion {
            identity: ReleaseTrialIdentity {
                trial_key: trial.trial_key.clone(),
                variant_key: trial.variant_key.clone(),
            },
            status: trial.status,
        })
        .collect::<Vec<_>>();
    if let Err(error) =
        require_completed_release_trial_set(run.status, run.expected_trials, &expected, &observed)
    {
        let trial_uids = trials
            .iter()
            .map(|trial| trial.trial_uid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let reason = error.to_string();
        return Ok(inconclusive_completion_decision(
            run_uid,
            candidate_revision_uid,
            subject_digest,
            policy.blocking_assertions,
            trial_uids,
            &reason,
        ));
    }

    let scores =
        moa_experiments::app::scores(pool.clone(), ExperimentScoresRequest { tenant_id, run_uid })
            .await
            .map_err(|error| Error::Storage(error.to_string()))?;
    let exact_rows = exact_experiment_run_score_rows_for_tenant(
        &pool,
        ExperimentRunScoreRowsRef {
            tenant_id,
            experiment_run_uid: run_uid,
        },
    )
    .await
    .map_err(|error| Error::Storage(error.to_string()))?;

    let trial_uids = scores
        .trials
        .iter()
        .map(|trial| trial.trial_uid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let evidence_ids = exact_rows
        .iter()
        .map(|row| row.score_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut gate_results = BTreeMap::new();

    let candidate_eligibility = variant_eligibility(
        &scores.variant_scorecards,
        ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
    );
    let initial_activation = candidate.subject.serving_baseline.is_none();
    let baseline_eligibility = if initial_activation {
        None
    } else {
        variant_eligibility(
            &scores.variant_scorecards,
            ARTIFACT_RELEASE_BASELINE_VARIANT_KEY,
        )
    };
    let baseline_scorecard_label = if initial_activation {
        "not_applicable"
    } else {
        eligibility_label(baseline_eligibility)
    };
    gate_results.insert(
        "candidate.scorecard".to_string(),
        eligibility_label(candidate_eligibility).to_string(),
    );
    gate_results.insert(
        "baseline.scorecard".to_string(),
        baseline_scorecard_label.to_string(),
    );
    let decision_contract = if initial_activation {
        "initial_activation_absolute_gate"
    } else {
        "absolute_activation_with_diagnostic_comparison_v1"
    };
    gate_results.insert(
        "release.decision_contract".to_string(),
        decision_contract.to_string(),
    );

    let assertion_verdict = blocking_assertion_verdict(
        &scores.trials,
        &policy.blocking_assertions,
        &mut gate_results,
    );
    let (mode_verdict, comparative_detail) = if initial_activation {
        gate_results.insert(
            "release.comparative_authority".to_string(),
            "not_applicable".to_string(),
        );
        (
            initial_activation_variant_verdict(&scores.trials),
            json!({
                "mode": "initial_activation_absolute_gate",
                "comparative_metrics": "skipped_no_serving_baseline",
            }),
        )
    } else {
        gate_results.insert(
            "release.comparative_authority".to_string(),
            "diagnostic_only".to_string(),
        );
        let detail = metric_family_analysis(
            &scores.trials,
            &policy.primary_gate_family,
            subject_digest,
            &mut gate_results,
        );
        (DeterministicVerdict::Pass, detail)
    };
    let verdict = combine_verdicts(&[
        absolute_eligibility_verdict(candidate_eligibility),
        mode_verdict,
        assertion_verdict,
    ]);
    let detail = json!({
        "decision_contract": decision_contract,
        "run_uid": run_uid,
        "candidate_revision_uid": candidate_revision_uid,
        "subject_digest": subject_digest,
        "candidate_scorecard": eligibility_label(candidate_eligibility),
        "baseline_scorecard": baseline_scorecard_label,
        "comparative_analysis": comparative_detail,
        "gate_results": gate_results,
        "trial_count": trial_uids.len(),
        "evidence_count": evidence_ids.len(),
    });
    Ok(ExperimentReleaseDecision {
        verdict,
        trial_uids,
        evidence_ids,
        gate_results,
        blocking_assertions: policy.blocking_assertions,
        detail,
    })
}

fn inconclusive_completion_decision(
    run_uid: Uuid,
    candidate_revision_uid: Uuid,
    subject_digest: Digest32,
    blocking_assertions: Vec<AssertionRef>,
    trial_uids: Vec<Uuid>,
    reason: &str,
) -> ExperimentReleaseDecision {
    let gate_results = BTreeMap::from([
        ("release.completion".to_string(), "inconclusive".to_string()),
        (
            "release.decision_contract".to_string(),
            "completed_exact_trial_set_required".to_string(),
        ),
    ]);
    let detail = json!({
        "decision_contract": "completed_exact_trial_set_required",
        "run_uid": run_uid,
        "candidate_revision_uid": candidate_revision_uid,
        "subject_digest": subject_digest,
        "completion_admission": "inconclusive",
        "reason": reason,
        "gate_results": gate_results,
        "trial_count": trial_uids.len(),
        "evidence_count": 0,
    });
    ExperimentReleaseDecision {
        verdict: DeterministicVerdict::Inconclusive,
        trial_uids,
        evidence_ids: Vec::new(),
        gate_results,
        blocking_assertions,
        detail,
    }
}

fn variant_eligibility(
    rollups: &[moa_core::types::experiments::ScorecardGroupRollup],
    key: &str,
) -> Option<ScorecardEligibility> {
    rollups
        .iter()
        .find(|rollup| rollup.key == key)
        .map(|rollup| rollup.eligibility)
}

fn eligibility_label(eligibility: Option<ScorecardEligibility>) -> &'static str {
    eligibility.map_or("missing", ScorecardEligibility::as_str)
}

fn absolute_eligibility_verdict(candidate: Option<ScorecardEligibility>) -> DeterministicVerdict {
    match candidate {
        Some(ScorecardEligibility::Eligible) => DeterministicVerdict::Pass,
        Some(ScorecardEligibility::Ineligible) => DeterministicVerdict::Regression,
        Some(ScorecardEligibility::Incomplete | ScorecardEligibility::Invalid) | None => {
            DeterministicVerdict::Inconclusive
        }
    }
}

fn initial_activation_variant_verdict(
    trials: &[ExperimentTrialScoreSummary],
) -> DeterministicVerdict {
    if trials.is_empty()
        || trials
            .iter()
            .any(|trial| trial.variant_key != ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY)
    {
        DeterministicVerdict::Inconclusive
    } else {
        DeterministicVerdict::Pass
    }
}

fn blocking_assertion_verdict(
    trials: &[ExperimentTrialScoreSummary],
    assertions: &[AssertionRef],
    results: &mut BTreeMap<String, String>,
) -> DeterministicVerdict {
    let candidate_trials = trials
        .iter()
        .filter(|trial| trial.variant_key == ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY)
        .collect::<Vec<_>>();
    if candidate_trials.is_empty() {
        return DeterministicVerdict::Inconclusive;
    }
    let mut missing = false;
    let mut regression = false;
    for assertion in assertions {
        let values = candidate_trials
            .iter()
            .filter_map(|trial| score_value(trial, &assertion.id))
            .collect::<Vec<_>>();
        let outcome = if values.len() != candidate_trials.len() {
            missing = true;
            "inconclusive"
        } else if values.iter().all(|value| *value >= 1.0) {
            "pass"
        } else {
            regression = true;
            "regression"
        };
        results.insert(assertion.id.clone(), outcome.to_string());
    }
    if regression {
        DeterministicVerdict::Regression
    } else if missing {
        DeterministicVerdict::Inconclusive
    } else {
        DeterministicVerdict::Pass
    }
}

fn metric_family_analysis(
    trials: &[ExperimentTrialScoreSummary],
    metrics: &[GateMetric],
    subject_digest: Digest32,
    results: &mut BTreeMap<String, String>,
) -> Value {
    let mut decisions = Vec::new();
    let mut reports = Vec::new();
    for (index, metric) in metrics.iter().enumerate() {
        let paired = paired_metric_observations(trials, &metric.metric);
        let definition = metric_definition(metric, gate_seed(subject_digest, index));
        let report = match (paired, definition) {
            (PairedMetricObservations::Numeric(observations), Ok(definition))
                if metric.confidence_method == GateConfidenceMethod::HierarchicalCaseBootstrap =>
            {
                evaluate_paired_numeric_gate(&definition, &observations)
                    .map_err(|error| error.to_string())
            }
            (PairedMetricObservations::Binary(observations), Ok(definition))
                if metric.confidence_method
                    == GateConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap =>
            {
                evaluate_paired_binary_gate(&definition, &observations)
                    .map_err(|error| error.to_string())
            }
            (PairedMetricObservations::Numeric(_), Ok(_)) => Err(format!(
                "primary gate metric {} produced numeric scores but declares a paired binary method",
                metric.metric
            )),
            (PairedMetricObservations::Binary(_), Ok(_)) => Err(format!(
                "primary gate metric {} produced binary scores but declares a hierarchical numeric method",
                metric.metric
            )),
            (PairedMetricObservations::Invalid(reason), _) => {
                results.insert(metric.metric.clone(), "inconclusive".to_string());
                reports.push(json!({"metric": metric.metric, "error": reason}));
                continue;
            }
            (_, Err(error)) => Err(error),
        };
        match report {
            Ok(report) => {
                let label = decision_label(report.decision.decision);
                results.insert(metric.metric.clone(), label.to_string());
                reports.push(serde_json::to_value(&report).unwrap_or_else(
                    |_| json!({"metric": metric.metric, "error": "report serialization failed"}),
                ));
                decisions.push(report.decision);
            }
            Err(error) => {
                results.insert(metric.metric.clone(), "inconclusive".to_string());
                reports.push(json!({"metric": metric.metric, "error": error}));
            }
        }
    }
    let gate = intersection_union_gate(&decisions);
    let missing_decision = decisions.len() != metrics.len();
    let regression_alpha = metrics
        .first()
        .and_then(|metric| metric.holm_regression_alpha_bp)
        .map(basis_points);
    let regression_family = regression_alpha
        .map(|alpha| holm_regression_family(&decisions, alpha))
        .unwrap_or_default();
    let diagnostic_verdict =
        comparative_family_verdict(&gate, &regression_family, missing_decision);
    json!({
        "authority": "diagnostic_only_pending_design_operating_characteristics",
        "activation_verdict_effect": "excluded",
        "diagnostic_verdict": deterministic_verdict_label(diagnostic_verdict),
        "gate": gate,
        "reports": reports,
        "regression_family": {
            "method": regression_alpha.map(|_| "holm"),
            "alpha": regression_alpha,
            "declarations": regression_family,
        },
        "support_interpretation": "diagnostic_floor_not_population_certification",
    })
}

fn comparative_family_verdict(
    gate: &GateOutcome,
    regression_family: &[RegressionDeclaration],
    missing_decision: bool,
) -> DeterministicVerdict {
    if missing_decision {
        return DeterministicVerdict::Inconclusive;
    }
    if gate.decision == Decision::Pass {
        return DeterministicVerdict::Pass;
    }
    if regression_family
        .iter()
        .any(|declaration| declaration.declared)
    {
        DeterministicVerdict::Regression
    } else {
        DeterministicVerdict::Inconclusive
    }
}

enum PairedMetricObservations {
    Numeric(Vec<PairedNumericObservation>),
    Binary(Vec<PairedBinaryObservation>),
    Invalid(String),
}

fn paired_metric_observations(
    trials: &[ExperimentTrialScoreSummary],
    metric: &str,
) -> PairedMetricObservations {
    let mut baseline = BTreeMap::new();
    let mut candidate = BTreeMap::new();
    let mut value_type = None;
    for trial in trials {
        let Some(row) = trial.rows.iter().find(|row| row.name == metric) else {
            continue;
        };
        let Some(value) = row.mean_or_rate else {
            continue;
        };
        if value_type.is_some_and(|existing| existing != row.value_type) {
            return PairedMetricObservations::Invalid(format!(
                "metric {metric} changed value type across paired trials"
            ));
        }
        value_type = Some(row.value_type);
        let key = pair_key(&trial.trial_key);
        let observation = (cluster_key(&trial.trial_key), value);
        match trial.variant_key.as_str() {
            ARTIFACT_RELEASE_BASELINE_VARIANT_KEY => {
                baseline.insert(key, observation);
            }
            ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY => {
                candidate.insert(key, observation);
            }
            _ => {}
        }
    }
    if baseline.keys().collect::<BTreeSet<_>>() != candidate.keys().collect::<BTreeSet<_>>()
        || baseline.is_empty()
    {
        return PairedMetricObservations::Invalid(format!(
            "metric {metric} does not have a complete candidate/baseline pairing"
        ));
    }
    match value_type {
        Some(ScorecardValueType::Boolean) => {
            if baseline
                .values()
                .chain(candidate.values())
                .any(|(_, value)| *value != 0.0 && *value != 1.0)
            {
                return PairedMetricObservations::Invalid(format!(
                    "primary gate metric {metric} aggregated multiple boolean outcomes per trial"
                ));
            }
            PairedMetricObservations::Binary(
                baseline
                    .into_iter()
                    .filter_map(|(pair_id, (cluster_id, baseline))| {
                        candidate
                            .get(&pair_id)
                            .map(|(_, candidate)| PairedBinaryObservation {
                                cluster_id,
                                pair_id,
                                baseline: baseline == 1.0,
                                candidate: *candidate == 1.0,
                            })
                    })
                    .collect(),
            )
        }
        Some(ScorecardValueType::Numeric) => PairedMetricObservations::Numeric(
            baseline
                .into_iter()
                .filter_map(|(pair_id, (cluster_id, baseline))| {
                    candidate
                        .get(&pair_id)
                        .map(|(_, candidate)| PairedNumericObservation {
                            cluster_id,
                            pair_id,
                            baseline,
                            candidate: *candidate,
                        })
                })
                .collect(),
        ),
        Some(ScorecardValueType::Categorical) => PairedMetricObservations::Invalid(format!(
            "primary gate metric {metric} is categorical"
        )),
        None => PairedMetricObservations::Invalid(format!(
            "primary gate metric {metric} has no persisted score rows"
        )),
    }
}

fn metric_definition(metric: &GateMetric, seed: u64) -> Result<MetricDefinition, String> {
    let direction = match metric.direction {
        MetricDirection::HigherIsBetter => EvalMetricDirection::HigherIsBetter,
        MetricDirection::LowerIsBetter => EvalMetricDirection::LowerIsBetter,
    };
    let margin = basis_points(metric.margin_bp);
    if margin <= 0.0 {
        return Err(format!(
            "primary gate metric {} has a non-positive non-inferiority margin",
            metric.metric
        ));
    }
    let resamples = usize::try_from(metric.resamples)
        .map_err(|_| format!("metric {} resample count exceeds usize", metric.metric))?;
    let min_independent_units = usize::try_from(metric.min_independent_units)
        .map_err(|_| format!("metric {} support floor exceeds usize", metric.metric))?;
    let plan = ResamplingPlan {
        resamples,
        seed,
        min_independent_units,
    };
    let (class, estimator, confidence_method) = match metric.confidence_method {
        GateConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap => (
            MetricClass::PairedBinary,
            Estimator::MatchedRiskDifference,
            ConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap(plan),
        ),
        GateConfidenceMethod::HierarchicalCaseBootstrap => (
            MetricClass::StochasticLive,
            Estimator::MeanPairedCaseDelta,
            ConfidenceMethod::HierarchicalCaseBootstrap(plan),
        ),
    };
    Ok(MetricDefinition {
        id: metric.metric.clone(),
        direction,
        estimand: Estimand {
            class,
            summary: metric.estimand.clone(),
            target_population: metric.target_population.clone(),
        },
        unit: match metric.unit {
            GateMetricUnit::Proportion => MetricUnit::Proportion,
        },
        independent_unit: metric.independent_unit.clone(),
        cluster_key: Some(metric.cluster_key.clone()),
        paired_key: Some(metric.paired_key.clone()),
        estimator,
        practical_margin: Some(margin),
        alpha: basis_points(metric.alpha_bp),
        confidence_method,
        acceptable_alternative: Some(basis_points(metric.acceptable_alternative_bp)),
        unacceptable_alternative: Some(basis_points(metric.unacceptable_alternative_bp)),
        gate_kind: GateKind::RequiredNonInferiority,
        hypothesis_family: HypothesisFamily::Primary,
    })
}

fn basis_points(value: impl Into<f64>) -> f64 {
    value.into() / 10_000.0
}

fn score_value(trial: &ExperimentTrialScoreSummary, name: &str) -> Option<f64> {
    trial
        .rows
        .iter()
        .find(|row| row.name == name)
        .and_then(|row| row.mean_or_rate)
}

fn pair_key(trial_key: &str) -> String {
    let Some((prefix, rest)) = trial_key.split_once("/v-") else {
        return trial_key.to_string();
    };
    let suffix = rest.split_once('/').map_or("", |(_, suffix)| suffix);
    format!("{prefix}/{suffix}")
}

fn cluster_key(trial_key: &str) -> String {
    trial_key
        .split_once("/v-")
        .map_or_else(|| trial_key.to_string(), |(prefix, _)| prefix.to_string())
}

fn gate_seed(digest: Digest32, index: usize) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(bytes) ^ u64::try_from(index).map_or(0, |value| value)
}

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Pass => "pass",
        Decision::Regression => "regression",
        Decision::Inconclusive => "inconclusive",
    }
}

fn deterministic_verdict_label(verdict: DeterministicVerdict) -> &'static str {
    match verdict {
        DeterministicVerdict::Pass => "pass",
        DeterministicVerdict::Regression => "regression",
        DeterministicVerdict::Inconclusive => "inconclusive",
    }
}

fn combine_verdicts(verdicts: &[DeterministicVerdict]) -> DeterministicVerdict {
    if verdicts.contains(&DeterministicVerdict::Regression) {
        DeterministicVerdict::Regression
    } else if verdicts.contains(&DeterministicVerdict::Inconclusive) {
        DeterministicVerdict::Inconclusive
    } else {
        DeterministicVerdict::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial_identity(key: &str, variant: &str) -> ReleaseTrialIdentity {
        ReleaseTrialIdentity {
            trial_key: key.to_string(),
            variant_key: variant.to_string(),
        }
    }

    fn completed_trial(key: &str, variant: &str) -> ReleaseTrialCompletion {
        ReleaseTrialCompletion {
            identity: trial_identity(key, variant),
            status: ExperimentTrialStatus::Completed,
        }
    }

    // Pins: release authority requires the completed parent and every exact
    // provisioned arm/case/repetition key; a terminal partial prefix cannot
    // become decision-ready merely because its surviving score rows pass.
    #[test]
    fn release_decision_requires_completed_run_and_exact_trial_set_offline() {
        let expected = [
            trial_identity("case-1/v-release_candidate/t001", "release_candidate"),
            trial_identity("case-1/v-release_baseline/t001", "release_baseline"),
        ];
        let complete = [
            completed_trial("case-1/v-release_candidate/t001", "release_candidate"),
            completed_trial("case-1/v-release_baseline/t001", "release_baseline"),
        ];
        assert!(
            require_completed_release_trial_set(
                ExperimentRunStatus::Completed,
                2,
                &expected,
                &complete,
            )
            .is_ok(),
            "the exact completed trial set should be decision-ready"
        );

        for status in [ExperimentRunStatus::Failed, ExperimentRunStatus::Cancelled] {
            assert!(
                require_completed_release_trial_set(status, 2, &expected, &complete).is_err(),
                "a {status:?} experiment must never be decision-ready"
            );
        }
        assert!(
            require_completed_release_trial_set(
                ExperimentRunStatus::Completed,
                2,
                &expected,
                &complete[..1],
            )
            .is_err(),
            "a passing terminal prefix must remain inconclusive"
        );
        assert!(
            require_completed_release_trial_set(
                ExperimentRunStatus::Completed,
                1,
                &expected,
                &complete,
            )
            .is_err(),
            "the run admission count must equal the provisioned set"
        );
        assert!(
            require_completed_release_trial_set(ExperimentRunStatus::Completed, 0, &[], &[],)
                .is_err(),
            "an empty release run cannot become decision-ready"
        );

        let wrong_arm = [
            completed_trial("case-1/v-release_candidate/t001", "release_candidate"),
            completed_trial("case-1/v-release_baseline/t001", "release_candidate"),
        ];
        assert!(
            require_completed_release_trial_set(
                ExperimentRunStatus::Completed,
                2,
                &expected,
                &wrong_arm,
            )
            .is_err(),
            "a trial key bound to the wrong arm must be refused"
        );

        let rejected = inconclusive_completion_decision(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Digest32([3; 32]),
            Vec::new(),
            vec![Uuid::from_u128(4)],
            "partial trial set",
        );
        assert_eq!(rejected.verdict, DeterministicVerdict::Inconclusive);
        assert!(rejected.evidence_ids.is_empty());
        assert_eq!(rejected.detail["completion_admission"], "inconclusive");
    }

    fn trial(case: usize, variant: &str, rows: &[(&str, f64)]) -> ExperimentTrialScoreSummary {
        ExperimentTrialScoreSummary {
            trial_uid: Uuid::now_v7(),
            trial_key: format!("s{case:02}-case/p01-persona/u01-profile/v-{variant}/t001"),
            score_run_id: Uuid::now_v7(),
            variant_key: variant.to_string(),
            scenario_id: Some(format!("case-{case}")),
            rows: rows
                .iter()
                .map(
                    |(name, value)| moa_wire::experiments::ExperimentScoreSummaryRow {
                        name: (*name).to_string(),
                        value_type: ScorecardValueType::Boolean,
                        n: 1,
                        mean_or_rate: Some(*value),
                    },
                )
                .collect(),
            eligibility: ScorecardEligibility::Eligible,
            eligibility_findings: Vec::new(),
        }
    }

    fn gate_metric(min_independent_units: u32) -> GateMetric {
        GateMetric {
            metric: "result_produced".to_string(),
            direction: MetricDirection::HigherIsBetter,
            estimand: "paired difference in result-production probability".to_string(),
            target_population: "approved artifact-release scenarios".to_string(),
            independent_unit: "scenario_persona_profile".to_string(),
            cluster_key: "scenario_persona_profile".to_string(),
            paired_key: "scenario_persona_profile_repetition".to_string(),
            confidence_method: GateConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap,
            unit: GateMetricUnit::Proportion,
            margin_bp: 500,
            alpha_bp: 250,
            acceptable_alternative_bp: 0,
            unacceptable_alternative_bp: -1_000,
            resamples: 2_000,
            min_independent_units,
            holm_regression_alpha_bp: Some(250),
        }
    }

    // Pins: release pairing removes only the arm variant coordinate and retains
    // scenario, persona, profile, and repetition as the exact pair identity.
    #[test]
    fn release_pair_key_preserves_every_non_variant_coordinate_offline() {
        let baseline = "s01-a/p01-b/u01-c/v-release_baseline/t003";
        let candidate = "s01-a/p01-b/u01-c/v-release_candidate/t003";
        assert_eq!(pair_key(baseline), pair_key(candidate));
        assert_eq!(pair_key(candidate), "s01-a/p01-b/u01-c/t003");
        assert_eq!(cluster_key(candidate), "s01-a/p01-b/u01-c");
    }

    // Pins: a first activation has no honest comparative arm. Candidate-only
    // deterministic evidence may pass, while a fabricated baseline fails the
    // absolute-gate shape instead of entering paired statistics.
    #[test]
    fn initial_activation_uses_candidate_only_absolute_gate_offline() {
        let candidate = trial(
            1,
            ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
            &[("scenario_outcome", 1.0)],
        );
        assert_eq!(
            initial_activation_variant_verdict(std::slice::from_ref(&candidate)),
            DeterministicVerdict::Pass
        );
        assert_eq!(
            absolute_eligibility_verdict(Some(ScorecardEligibility::Eligible)),
            DeterministicVerdict::Pass
        );

        let fabricated_baseline = trial(
            1,
            ARTIFACT_RELEASE_BASELINE_VARIANT_KEY,
            &[("scenario_outcome", 1.0)],
        );
        assert_eq!(
            initial_activation_variant_verdict(&[candidate, fabricated_baseline]),
            DeterministicVerdict::Inconclusive
        );
    }

    #[test]
    fn deterministic_scores_are_authoritative_while_comparison_is_diagnostic_offline() {
        // Pins: the default activation contract is expressed in deterministic
        // score rows the production evaluator emits. Comparative inference is
        // still calculated, but it cannot grant or deny activation before its
        // exact design has passed an operating-characteristic assessment.
        let assertions = moa_artifacts::release::PLATFORM_BLOCKING_ASSERTIONS
            .iter()
            .map(|id| AssertionRef {
                id: (*id).to_string(),
                version: "v1".to_string(),
                determinism: moa_artifacts::release::DeterminismClass::Deterministic,
            })
            .collect::<Vec<_>>();
        let rows = [
            ("scenario_outcome", 1.0),
            ("target_completed", 1.0),
            ("result_produced", 1.0),
            ("privacy_safe_output", 1.0),
        ];
        let mut trials = Vec::new();
        for case in 1..=6 {
            trials.push(trial(case, ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, &rows));
            trials.push(trial(case, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY, &rows));
        }
        let mut results = BTreeMap::new();
        assert_eq!(
            blocking_assertion_verdict(&trials, &assertions, &mut results),
            DeterministicVerdict::Pass
        );
        let metrics = [gate_metric(6)];
        let analysis = metric_family_analysis(&trials, &metrics, Digest32([7; 32]), &mut results);
        assert_eq!(
            analysis["diagnostic_verdict"], "pass",
            "the comparison remains useful as a diagnostic"
        );
        assert_eq!(
            analysis["authority"],
            "diagnostic_only_pending_design_operating_characteristics"
        );
        assert_eq!(analysis["activation_verdict_effect"], "excluded");

        trials
            .iter_mut()
            .find(|trial| trial.variant_key == ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY)
            .and_then(|trial| {
                trial
                    .rows
                    .iter_mut()
                    .find(|row| row.name == "privacy_safe_output")
            })
            .expect("candidate privacy row")
            .mean_or_rate = Some(0.0);
        assert_eq!(
            blocking_assertion_verdict(&trials, &assertions, &mut BTreeMap::new()),
            DeterministicVerdict::Regression
        );

        trials
            .iter_mut()
            .filter(|trial| trial.variant_key == ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY)
            .for_each(|trial| {
                trial
                    .rows
                    .iter_mut()
                    .find(|row| row.name == "privacy_safe_output")
                    .expect("candidate privacy row")
                    .mean_or_rate = Some(1.0);
            });
        trials
            .iter_mut()
            .find(|trial| trial.variant_key == ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY)
            .and_then(|trial| {
                trial
                    .rows
                    .iter_mut()
                    .find(|row| row.name == "scenario_outcome")
            })
            .expect("candidate scenario outcome")
            .mean_or_rate = Some(0.0);
        assert_eq!(
            blocking_assertion_verdict(&trials, &assertions, &mut BTreeMap::new()),
            DeterministicVerdict::Regression
        );
    }

    // Pins: the policy's independent-case floor, not repetitions, determines
    // whether even the diagnostic paired analysis has enough population support.
    #[test]
    fn five_clusters_remain_diagnostically_inconclusive_despite_repetitions_offline() {
        let rows = [("result_produced", 1.0)];
        let mut trials = Vec::new();
        for case in 1..=5 {
            for repetition in 1..=4 {
                let mut baseline = trial(case, ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, &rows);
                baseline.trial_key = baseline
                    .trial_key
                    .replace("t001", &format!("t{repetition:03}"));
                let mut candidate = trial(case, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY, &rows);
                candidate.trial_key = candidate
                    .trial_key
                    .replace("t001", &format!("t{repetition:03}"));
                trials.extend([baseline, candidate]);
            }
        }

        let detail = metric_family_analysis(
            &trials,
            &[gate_metric(6)],
            Digest32([8; 32]),
            &mut BTreeMap::new(),
        );
        assert_eq!(detail["diagnostic_verdict"], "inconclusive");
        assert_eq!(
            detail["reports"][0]["decision"]["support"]["independent_units"],
            5
        );
        assert_eq!(
            detail["reports"][0]["decision"]["support"]["required_independent_units"],
            6
        );
        assert_eq!(
            detail["support_interpretation"],
            "diagnostic_floor_not_population_certification"
        );
    }

    // Pins: an unadjusted metric regression cannot leak through the IUT gate as
    // an overall release regression; Holm must declare the reverse hypothesis.
    #[test]
    fn release_overall_regression_requires_a_holm_declaration_offline() {
        let gate = GateOutcome {
            family: moa_eval_core::decision::GateFamily::IntersectionUnionNonInferiority,
            decision: Decision::Regression,
            passing: Vec::new(),
            regressed: vec!["result_produced".to_string()],
            inconclusive: Vec::new(),
            rationale: "raw metric regression".to_string(),
        };
        let not_declared = [RegressionDeclaration {
            metric_id: "result_produced".to_string(),
            raw_p_value: 0.03,
            adjusted_p_value: 0.06,
            declared: false,
        }];
        assert_eq!(
            comparative_family_verdict(&gate, &not_declared, false),
            DeterministicVerdict::Inconclusive
        );

        let declared = [RegressionDeclaration {
            declared: true,
            ..not_declared[0].clone()
        }];
        assert_eq!(
            comparative_family_verdict(&gate, &declared, false),
            DeterministicVerdict::Regression
        );
    }
}
