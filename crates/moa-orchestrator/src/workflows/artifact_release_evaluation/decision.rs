//! Deterministic release decisions derived from production experiment evidence.
//!
//! This module does not run another evaluator. It reads the exact trial rows and
//! score provenance produced by `ExperimentTrialRun`, pairs the candidate and
//! baseline variants on their plan coordinates, applies the predeclared release
//! policy with MOA's existing paired non-inferiority machinery, and returns the
//! evidence identifiers the release transaction must attest.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::registry::ReleaseRepository;
use moa_artifacts::release::{
    AssertionRef, DeterministicVerdict, Digest32, GateMetric, MetricDirection, TenantScope,
};
use moa_core::types::experiments::{ScorecardEligibility, ScorecardValueType};
use moa_core::types::identifiers::TenantId;
use moa_eval::kernel::stats::{
    PairedBinaryObservation, PairedNumericObservation, evaluate_paired_binary_gate,
    evaluate_paired_numeric_gate,
};
use moa_eval_core::decision::{Decision, intersection_union_gate};
use moa_eval_core::metric::{
    ConfidenceMethod, Estimand, Estimator, GateKind, HypothesisFamily, MetricClass,
    MetricDefinition, MetricDirection as EvalMetricDirection, MetricUnit, ResamplingPlan,
};
use moa_scoring::{ExperimentRunScoreRowsRef, exact_experiment_run_score_rows_for_tenant};
use moa_wire::experiments::{
    ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
    ExperimentScoresRequest, ExperimentTrialScoreSummary,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::Error;

const RELEASE_GATE_ALPHA: f64 = 0.025;
const RELEASE_GATE_RESAMPLES: usize = 2_000;
const RELEASE_GATE_MIN_CASES: usize = 4;

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
    let baseline_eligibility = variant_eligibility(
        &scores.variant_scorecards,
        ARTIFACT_RELEASE_BASELINE_VARIANT_KEY,
    );
    gate_results.insert(
        "candidate.scorecard".to_string(),
        eligibility_label(candidate_eligibility).to_string(),
    );
    gate_results.insert(
        "baseline.scorecard".to_string(),
        eligibility_label(baseline_eligibility).to_string(),
    );

    let assertion_verdict = blocking_assertion_verdict(
        &scores.trials,
        &policy.blocking_assertions,
        &mut gate_results,
    );
    let (metric_verdict, metric_detail) = metric_family_verdict(
        &scores.trials,
        &policy.primary_gate_family,
        subject_digest,
        &mut gate_results,
    );
    let verdict = combine_verdicts(
        eligibility_verdict(candidate_eligibility, baseline_eligibility),
        assertion_verdict,
        metric_verdict,
    );
    let detail = json!({
        "decision_contract": "artifact_release_experiment_v1",
        "run_uid": run_uid,
        "candidate_revision_uid": candidate_revision_uid,
        "subject_digest": subject_digest,
        "candidate_scorecard": eligibility_label(candidate_eligibility),
        "baseline_scorecard": eligibility_label(baseline_eligibility),
        "metric_gate": metric_detail,
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

fn eligibility_verdict(
    candidate: Option<ScorecardEligibility>,
    baseline: Option<ScorecardEligibility>,
) -> DeterministicVerdict {
    match (candidate, baseline) {
        (Some(ScorecardEligibility::Eligible), Some(ScorecardEligibility::Eligible)) => {
            DeterministicVerdict::Pass
        }
        (Some(ScorecardEligibility::Ineligible), Some(ScorecardEligibility::Eligible)) => {
            DeterministicVerdict::Regression
        }
        _ => DeterministicVerdict::Inconclusive,
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

fn metric_family_verdict(
    trials: &[ExperimentTrialScoreSummary],
    metrics: &[GateMetric],
    subject_digest: Digest32,
    results: &mut BTreeMap<String, String>,
) -> (DeterministicVerdict, Value) {
    let mut decisions = Vec::new();
    let mut reports = Vec::new();
    for (index, metric) in metrics.iter().enumerate() {
        let paired = paired_metric_observations(trials, &metric.metric);
        let report = match paired {
            PairedMetricObservations::Numeric(observations) => metric_definition(
                metric,
                gate_seed(subject_digest, index),
                GateObservationKind::Numeric,
            )
            .and_then(|definition| {
                evaluate_paired_numeric_gate(&definition, &observations)
                    .map_err(|error| error.to_string())
            }),
            PairedMetricObservations::Binary(observations) => metric_definition(
                metric,
                gate_seed(subject_digest, index),
                GateObservationKind::Binary,
            )
            .and_then(|definition| {
                evaluate_paired_binary_gate(&definition, &observations)
                    .map_err(|error| error.to_string())
            }),
            PairedMetricObservations::Invalid(reason) => {
                results.insert(metric.metric.clone(), "inconclusive".to_string());
                reports.push(json!({"metric": metric.metric, "error": reason}));
                continue;
            }
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
    let verdict = if missing_decision {
        DeterministicVerdict::Inconclusive
    } else {
        match gate.decision {
            Decision::Pass => DeterministicVerdict::Pass,
            Decision::Regression => DeterministicVerdict::Regression,
            Decision::Inconclusive => DeterministicVerdict::Inconclusive,
        }
    };
    (verdict, json!({"gate": gate, "reports": reports}))
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

#[derive(Clone, Copy)]
enum GateObservationKind {
    Numeric,
    Binary,
}

fn metric_definition(
    metric: &GateMetric,
    seed: u64,
    kind: GateObservationKind,
) -> Result<MetricDefinition, String> {
    let direction = match metric.direction {
        MetricDirection::HigherIsBetter => EvalMetricDirection::HigherIsBetter,
        MetricDirection::LowerIsBetter => EvalMetricDirection::LowerIsBetter,
    };
    let margin = f64::from(metric.margin_bp) / 10_000.0;
    if margin <= 0.0 {
        return Err(format!(
            "primary gate metric {} has a non-positive non-inferiority margin",
            metric.metric
        ));
    }
    let (class, estimator, confidence_method) = match kind {
        GateObservationKind::Numeric => (
            MetricClass::StochasticLive,
            Estimator::MeanPairedCaseDelta,
            ConfidenceMethod::HierarchicalCaseBootstrap(ResamplingPlan {
                resamples: RELEASE_GATE_RESAMPLES,
                seed,
                min_independent_units: RELEASE_GATE_MIN_CASES,
            }),
        ),
        GateObservationKind::Binary => (
            MetricClass::PairedBinary,
            Estimator::MatchedRiskDifference,
            ConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap(ResamplingPlan {
                resamples: RELEASE_GATE_RESAMPLES,
                seed,
                min_independent_units: RELEASE_GATE_MIN_CASES,
            }),
        ),
    };
    Ok(MetricDefinition {
        id: metric.metric.clone(),
        direction,
        estimand: Estimand {
            class,
            summary: format!("paired release delta for {}", metric.metric),
            target_population: "approved artifact-release case pack".to_string(),
        },
        unit: MetricUnit::Proportion,
        independent_unit: "release_case".to_string(),
        cluster_key: Some("scenario_persona_profile".to_string()),
        paired_key: Some("plan_trial_without_variant".to_string()),
        estimator,
        practical_margin: Some(margin),
        alpha: RELEASE_GATE_ALPHA,
        confidence_method,
        acceptable_alternative: Some(0.0),
        unacceptable_alternative: Some(-2.0 * margin),
        gate_kind: GateKind::RequiredNonInferiority,
        hypothesis_family: HypothesisFamily::Primary,
    })
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

fn combine_verdicts(
    first: DeterministicVerdict,
    second: DeterministicVerdict,
    third: DeterministicVerdict,
) -> DeterministicVerdict {
    if [first, second, third].contains(&DeterministicVerdict::Regression) {
        DeterministicVerdict::Regression
    } else if [first, second, third].contains(&DeterministicVerdict::Inconclusive) {
        DeterministicVerdict::Inconclusive
    } else {
        DeterministicVerdict::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn production_deterministic_scores_can_pass_the_release_gate_offline() {
        // Pins: the default release contract is expressed in score rows the
        // production trial evaluator actually emits. Complete passing rows can
        // resolve to pass; a missing or false blocking row still fails closed.
        let assertions = vec![
            AssertionRef {
                id: "target_completed".to_string(),
                version: "v1".to_string(),
                determinism: moa_artifacts::release::DeterminismClass::Deterministic,
            },
            AssertionRef {
                id: "result_produced".to_string(),
                version: "v1".to_string(),
                determinism: moa_artifacts::release::DeterminismClass::Deterministic,
            },
            AssertionRef {
                id: "privacy_safe_output".to_string(),
                version: "v1".to_string(),
                determinism: moa_artifacts::release::DeterminismClass::Deterministic,
            },
        ];
        let rows = [
            ("target_completed", 1.0),
            ("result_produced", 1.0),
            ("privacy_safe_output", 1.0),
        ];
        let mut trials = Vec::new();
        for case in 1..=4 {
            trials.push(trial(case, ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, &rows));
            trials.push(trial(case, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY, &rows));
        }
        let mut results = BTreeMap::new();
        assert_eq!(
            blocking_assertion_verdict(&trials, &assertions, &mut results),
            DeterministicVerdict::Pass
        );
        let metrics = [GateMetric {
            metric: "result_produced".to_string(),
            direction: MetricDirection::HigherIsBetter,
            margin_bp: 500,
        }];
        assert_eq!(
            metric_family_verdict(&trials, &metrics, Digest32([7; 32]), &mut results).0,
            DeterministicVerdict::Pass
        );

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
    }
}
