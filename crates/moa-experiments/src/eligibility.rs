//! Behavior Lab scorecard completeness policy.
//!
//! `moa-scoring` owns the raw exact-row query. This module owns what those rows
//! have to look like: exactly one row for every typed blocking requirement, with
//! evaluator, version, value type, plan revision, trial, target, and evidence
//! hash all matching what the trial actually ran. Wrong or duplicate rows never
//! satisfy the gate.
//!
//! This is Behavior Lab scorecard eligibility. It is not an agent deployment
//! guard, and nothing here should be treated as one until a deployment path
//! explicitly consumes it.

use moa_core::types::experiments::{ExperimentScorecard, ScorecardRequirement};
pub use moa_core::types::experiments::{
    ScorecardEligibility, ScorecardFinding, ScorecardGroupRollup,
};
use moa_scoring::ExperimentScoreRow;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::evaluator::{EvaluatorDescriptor, EvaluatorError, descriptor, validate_scorecard};
use crate::evidence::TrialScoreTarget;

/// The exact identity a trial's score rows must carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScorecardExpectation {
    /// Score run the trial writes into.
    pub score_run_id: Uuid,
    /// Experiment run that owns the trial.
    pub experiment_run_uid: Uuid,
    /// Pinned plan revision the trial ran.
    pub plan_revision_uid: Uuid,
    /// Trial the scores belong to.
    pub trial_uid: Uuid,
    /// Exact target the trial drove.
    pub target: TrialScoreTarget,
    /// BLAKE3 digest of the evidence the scores were derived from.
    pub evidence_hash: Vec<u8>,
}

/// One trial's scorecard assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScorecardAssessment {
    /// Overall eligibility.
    pub eligibility: ScorecardEligibility,
    /// Findings that produced anything other than `Eligible`, in requirement order.
    #[serde(default)]
    pub findings: Vec<ScorecardFinding>,
}

impl ScorecardAssessment {
    fn invalid(score_name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            eligibility: ScorecardEligibility::Invalid,
            findings: vec![ScorecardFinding {
                score_name: score_name.into(),
                detail: detail.into(),
            }],
        }
    }
}

/// Assesses one trial's exact score rows against its scorecard.
///
/// Informational requirements are never consulted: a failing informational
/// result cannot make a trial ineligible, and a missing one cannot make it
/// incomplete.
#[must_use]
pub fn assess_trial_scorecard(
    scorecard: &ExperimentScorecard,
    expectation: &ScorecardExpectation,
    rows: &[ExperimentScoreRow],
) -> ScorecardAssessment {
    if let Err(error) = validate_scorecard(scorecard) {
        return ScorecardAssessment::invalid("*", error.to_string());
    }

    let mut eligibility = ScorecardEligibility::Eligible;
    let mut findings = Vec::new();
    for requirement in scorecard.blocking_requirements() {
        let descriptor = match descriptor(&requirement.evaluator_id, &requirement.evaluator_version)
        {
            Ok(descriptor) => descriptor,
            Err(error) => return ScorecardAssessment::invalid("*", error.to_string()),
        };
        let matched = rows
            .iter()
            .filter(|row| row.name == descriptor.score_name)
            .collect::<Vec<_>>();
        let (outcome, detail) = assess_requirement(requirement, descriptor, expectation, &matched);
        if let Some(detail) = detail {
            findings.push(ScorecardFinding {
                score_name: descriptor.score_name.to_string(),
                detail,
            });
        }
        eligibility = eligibility.worst(outcome);
    }

    ScorecardAssessment {
        eligibility,
        findings,
    }
}

fn assess_requirement(
    requirement: &ScorecardRequirement,
    descriptor: &EvaluatorDescriptor,
    expectation: &ScorecardExpectation,
    matched: &[&ExperimentScoreRow],
) -> (ScorecardEligibility, Option<String>) {
    let row = match matched {
        [] => {
            return (
                ScorecardEligibility::Incomplete,
                Some("no provenance-backed score row is visible yet".to_string()),
            );
        }
        [row] => *row,
        rows => {
            return (
                ScorecardEligibility::Invalid,
                Some(format!(
                    "{} rows satisfy one requirement; exactly one is required",
                    rows.len()
                )),
            );
        }
    };

    if let Some(mismatch) = linkage_mismatch(requirement, descriptor, expectation, row) {
        return (ScorecardEligibility::Invalid, Some(mismatch));
    }

    match row.value_boolean {
        Some(true) => (ScorecardEligibility::Eligible, None),
        Some(false) => (
            ScorecardEligibility::Ineligible,
            Some("blocking evaluator returned false".to_string()),
        ),
        None => (
            ScorecardEligibility::Invalid,
            Some("blocking boolean requirement has no boolean value".to_string()),
        ),
    }
}

fn linkage_mismatch(
    requirement: &ScorecardRequirement,
    descriptor: &EvaluatorDescriptor,
    expectation: &ScorecardExpectation,
    row: &ExperimentScoreRow,
) -> Option<String> {
    let expected_value_type = descriptor.value_type.as_str();
    let checks: [(&str, String, String); 10] = [
        (
            "evaluator_id",
            requirement.evaluator_id.clone(),
            row.evaluator_id.clone(),
        ),
        (
            "evaluator_version",
            requirement.evaluator_version.clone(),
            row.evaluator_version.clone(),
        ),
        (
            "value_type",
            expected_value_type.to_string(),
            row.value_type.clone(),
        ),
        (
            "provenance_value_type",
            expected_value_type.to_string(),
            row.provenance_value_type.clone(),
        ),
        (
            "provenance_score_name",
            descriptor.score_name.to_string(),
            row.provenance_score_name.clone(),
        ),
        (
            "score_run_id",
            expectation.score_run_id.to_string(),
            row.score_run_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        ),
        (
            "experiment_run_uid",
            expectation.experiment_run_uid.to_string(),
            row.experiment_run_uid.to_string(),
        ),
        (
            "plan_revision_uid",
            expectation.plan_revision_uid.to_string(),
            row.plan_revision_uid.to_string(),
        ),
        (
            "trial_uid",
            expectation.trial_uid.to_string(),
            row.trial_uid.to_string(),
        ),
        (
            "target",
            expectation.target.identity_fragment(),
            row_target_fragment(row),
        ),
    ];
    for (field, expected, actual) in checks {
        if expected != actual {
            return Some(format!("{field} is `{actual}`, expected `{expected}`"));
        }
    }
    if row.evidence_hash != expectation.evidence_hash {
        return Some("evidence hash does not match the evidence this trial produced".to_string());
    }
    None
}

fn row_target_fragment(row: &ExperimentScoreRow) -> String {
    match (row.target_session_id, row.target_execution_run_uid) {
        (Some(session_id), None) => format!("session:{session_id}"),
        (None, Some(execution_run_uid)) => format!("execution_run:{execution_run_uid}"),
        _ => "<unattributable>".to_string(),
    }
}

/// Rolls per-trial assessments up into one group eligibility.
///
/// A group is only as eligible as its worst trial: one incomplete or ineligible
/// trial in a scenario means the scenario has not proven itself.
#[must_use]
pub fn roll_up_group(
    key: impl Into<String>,
    assessments: &[ScorecardEligibility],
) -> ScorecardGroupRollup {
    let eligibility = assessments
        .iter()
        .copied()
        .fold(ScorecardEligibility::Eligible, ScorecardEligibility::worst);
    ScorecardGroupRollup {
        key: key.into(),
        // A group with no trials has proven nothing, which is incomplete rather
        // than vacuously eligible.
        eligibility: if assessments.is_empty() {
            ScorecardEligibility::Incomplete
        } else {
            eligibility
        },
        trials: assessments.len(),
    }
}

/// Returns the evaluator error that makes a scorecard unrunnable, if any.
///
/// # Errors
///
/// Returns [`EvaluatorError`] when the scorecard names an evaluator, version,
/// effect, or configuration this build cannot run.
pub fn require_runnable_scorecard(scorecard: &ExperimentScorecard) -> Result<(), EvaluatorError> {
    validate_scorecard(scorecard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::experiments::ScorecardEffect;
    use moa_core::types::identifiers::SessionId;
    use serde_json::json;

    const EVIDENCE_HASH: [u8; 32] = [7; 32];

    fn scorecard() -> ExperimentScorecard {
        ExperimentScorecard::new(vec![
            ScorecardRequirement {
                evaluator_id: "target_completed".to_string(),
                evaluator_version: "v1".to_string(),
                config: json!({}),
                effect: ScorecardEffect::Blocking,
            },
            ScorecardRequirement {
                evaluator_id: "result_produced".to_string(),
                evaluator_version: "v1".to_string(),
                config: json!({}),
                effect: ScorecardEffect::Informational,
            },
        ])
        .expect("structurally valid scorecard")
    }

    fn expectation() -> ScorecardExpectation {
        ScorecardExpectation {
            score_run_id: Uuid::from_u128(1),
            experiment_run_uid: Uuid::from_u128(2),
            plan_revision_uid: Uuid::from_u128(3),
            trial_uid: Uuid::from_u128(4),
            target: TrialScoreTarget::Session {
                session_id: SessionId(Uuid::from_u128(5)),
            },
            evidence_hash: EVIDENCE_HASH.to_vec(),
        }
    }

    fn row(name: &str, value: bool) -> ExperimentScoreRow {
        ExperimentScoreRow {
            score_id: Uuid::from_u128(9),
            score_run_id: Some(Uuid::from_u128(1)),
            name: name.to_string(),
            value_type: "boolean".to_string(),
            value_numeric: None,
            value_boolean: Some(value),
            value_categorical: None,
            model_or_evaluator: "target_completed@v1".to_string(),
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            provenance_score_name: name.to_string(),
            provenance_value_type: "boolean".to_string(),
            experiment_run_uid: Uuid::from_u128(2),
            plan_revision_uid: Uuid::from_u128(3),
            trial_uid: Uuid::from_u128(4),
            target_session_id: Some(Uuid::from_u128(5)),
            target_execution_run_uid: None,
            evidence_ref:
                "session:00000000-0000-0000-0000-000000000005#seq=1&turns=1&outcome=completed"
                    .to_string(),
            evidence_hash: EVIDENCE_HASH.to_vec(),
        }
    }

    #[test]
    fn exactly_one_correct_passing_row_per_blocking_requirement_is_eligible_offline() {
        // Pins: the happy path, so every rejection below is a real rejection
        // rather than a gate that never returns Eligible at all.
        let assessment = assess_trial_scorecard(
            &scorecard(),
            &expectation(),
            &[row("target_completed", true)],
        );

        assert_eq!(assessment.eligibility, ScorecardEligibility::Eligible);
        assert!(assessment.findings.is_empty(), "{assessment:?}");
    }

    #[test]
    fn removing_one_required_evaluator_result_blocks_eligibility_offline() {
        // Pins: the headline acceptance criterion. Dropping the only row for a
        // blocking requirement moves the trial to Incomplete, never Eligible.
        let assessment = assess_trial_scorecard(&scorecard(), &expectation(), &[]);

        assert_eq!(assessment.eligibility, ScorecardEligibility::Incomplete);
        assert_eq!(assessment.findings.len(), 1);
        assert_eq!(assessment.findings[0].score_name, "target_completed");
    }

    #[test]
    fn duplicate_rows_never_satisfy_the_gate_offline() {
        // Pins: two rows for one requirement is a structural fault, not a
        // "close enough, one of them passed" pass.
        let assessment = assess_trial_scorecard(
            &scorecard(),
            &expectation(),
            &[row("target_completed", true), row("target_completed", true)],
        );

        assert_eq!(assessment.eligibility, ScorecardEligibility::Invalid);
        assert!(
            assessment.findings[0].detail.contains("2 rows"),
            "{:?}",
            assessment.findings
        );
    }

    #[test]
    fn a_row_from_another_run_plan_trial_or_target_never_satisfies_the_gate_offline() {
        // Pins: every linkage component is individually load-bearing, so a score
        // cannot be borrowed from a neighbouring score run, plan revision, trial,
        // session, or execution run.
        type RowMutation = (&'static str, fn(&mut ExperimentScoreRow));
        let mutations: [RowMutation; 6] = [
            ("score_run_id", |row| {
                row.score_run_id = Some(Uuid::from_u128(90));
            }),
            ("experiment_run_uid", |row| {
                row.experiment_run_uid = Uuid::from_u128(91);
            }),
            ("plan_revision_uid", |row| {
                row.plan_revision_uid = Uuid::from_u128(92);
            }),
            ("trial_uid", |row| {
                row.trial_uid = Uuid::from_u128(93);
            }),
            ("target", |row| {
                row.target_session_id = Some(Uuid::from_u128(94));
            }),
            ("target", |row| {
                row.target_session_id = None;
                row.target_execution_run_uid = Some(Uuid::from_u128(95));
            }),
        ];
        for (field, mutate) in mutations {
            let mut candidate = row("target_completed", true);
            mutate(&mut candidate);
            let assessment = assess_trial_scorecard(&scorecard(), &expectation(), &[candidate]);
            assert_eq!(
                assessment.eligibility,
                ScorecardEligibility::Invalid,
                "mislinked {field} was accepted"
            );
        }
    }

    #[test]
    fn evaluator_version_and_evidence_hash_are_identity_offline() {
        // Pins: a row produced by a different evaluator version, or derived from
        // different evidence, is not the row this requirement asked for.
        let mut wrong_version = row("target_completed", true);
        wrong_version.evaluator_version = "v2".to_string();
        assert_eq!(
            assess_trial_scorecard(&scorecard(), &expectation(), &[wrong_version]).eligibility,
            ScorecardEligibility::Invalid
        );

        let mut wrong_evidence = row("target_completed", true);
        wrong_evidence.evidence_hash = vec![9; 32];
        assert_eq!(
            assess_trial_scorecard(&scorecard(), &expectation(), &[wrong_evidence]).eligibility,
            ScorecardEligibility::Invalid
        );
    }

    #[test]
    fn a_failing_blocking_result_is_ineligible_and_informational_results_cannot_block_offline() {
        // Pins: blocking false makes the trial ineligible; an informational row
        // that is missing, false, or mislinked changes nothing.
        assert_eq!(
            assess_trial_scorecard(
                &scorecard(),
                &expectation(),
                &[row("target_completed", false)]
            )
            .eligibility,
            ScorecardEligibility::Ineligible
        );

        let mut bad_informational = row("result_produced", false);
        bad_informational.trial_uid = Uuid::from_u128(999);
        let assessment = assess_trial_scorecard(
            &scorecard(),
            &expectation(),
            &[row("target_completed", true), bad_informational],
        );
        assert_eq!(assessment.eligibility, ScorecardEligibility::Eligible);
        assert!(assessment.findings.is_empty());
    }

    #[test]
    fn a_scorecard_this_build_cannot_run_is_invalid_offline() {
        // Pins: an unknown evaluator makes the scorecard Invalid rather than
        // permanently Incomplete, which would look like "still waiting" forever.
        let unrunnable = ExperimentScorecard::new(vec![ScorecardRequirement {
            evaluator_id: "evaluator_from_the_future".to_string(),
            evaluator_version: "v1".to_string(),
            config: json!({}),
            effect: ScorecardEffect::Blocking,
        }])
        .expect("structurally valid");

        assert_eq!(
            assess_trial_scorecard(&unrunnable, &expectation(), &[]).eligibility,
            ScorecardEligibility::Invalid
        );
    }

    #[test]
    fn group_rollup_takes_the_worst_trial_and_refuses_to_be_vacuously_eligible_offline() {
        // Pins: a scenario is only as good as its worst trial, and a scenario with
        // no trials has proven nothing.
        assert_eq!(
            roll_up_group("scenario-a", &[]).eligibility,
            ScorecardEligibility::Incomplete
        );
        assert_eq!(
            roll_up_group(
                "scenario-a",
                &[
                    ScorecardEligibility::Eligible,
                    ScorecardEligibility::Eligible
                ]
            )
            .eligibility,
            ScorecardEligibility::Eligible
        );
        assert_eq!(
            roll_up_group(
                "scenario-a",
                &[
                    ScorecardEligibility::Eligible,
                    ScorecardEligibility::Ineligible,
                    ScorecardEligibility::Incomplete,
                ]
            )
            .eligibility,
            ScorecardEligibility::Incomplete
        );
        assert_eq!(
            roll_up_group(
                "scenario-a",
                &[
                    ScorecardEligibility::Incomplete,
                    ScorecardEligibility::Invalid,
                ]
            )
            .eligibility,
            ScorecardEligibility::Invalid
        );
    }
}
