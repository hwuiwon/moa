//! Builds the Behavior Lab request one release attempt dispatches.
//!
//! Dispatch goes through `Experiments/run -> ExperimentRun`, never through a
//! hosted `Eval` surface: that surface was deleted, and the durable experiment
//! workflows are the ones with tenant authorization, resource envelopes, and
//! trial-level persistence already attached.
//!
//! The request is built as a *single plan-backed paired run* rather than two
//! independent runs, and that is the mechanism behind the "same
//! case/persona/profile/repetition seeds" requirement. A plan-backed run expands
//! one pinned experiment plan into its trial matrix and then runs every declared
//! variant across that same matrix, so the candidate and the baseline see the same
//! cases, the same personas, the same profiles, and the same repetition indices by
//! construction. Two separate runs would each expand their own matrix and could
//! only be argued to match.

use moa_core::types::identifiers::TenantId;
use moa_wire::experiments::{
    ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
    ArtifactReleaseExperimentArm, ArtifactReleaseExperimentBinding,
    ArtifactReleaseExperimentTrialBinding, ExperimentRunRequest,
};

use super::Error;
use super::types::{ArmRole, DispatchRecord, ProvisionedAttempt};

/// Builds the paired experiment run request for one release attempt.
///
/// Three properties are asserted by construction rather than checked later:
///
/// * The request names only an immutable plan revision. Admission projects the
///   target and cannot accept a caller-supplied target alongside it.
/// * `idempotency_key` is the dispatch record's key, which is deterministic in
///   `(revision, generation, subject digest)`. A Restate replay of the same
///   attempt therefore re-admits the same run instead of starting a second one.
/// * Candidate and baseline are release variants of one run, which is what pairs
///   them on identical seeds for every artifact class.
pub fn build_paired_run_request(
    tenant_id: TenantId,
    record: &DispatchRecord,
    attempt: &ProvisionedAttempt,
) -> Result<ExperimentRunRequest, Error> {
    if !attempt.has_role(ArmRole::Candidate) {
        return Err(Error::Provisioning(
            "a release attempt has no candidate arm to run".to_string(),
        ));
    }
    Ok(ExperimentRunRequest {
        tenant_id,
        name: format!(
            "release:{}:{}:{}",
            attempt.activation_target, record.revision_uid, record.generation
        ),
        plan_revision_uid: attempt.plan.plan_revision_uid,
        score_run_id: None,
        idempotency_key: Some(record.idempotency_key.clone()),
        agent_revision_variants: Vec::new(),
        release_evaluation: Some(ArtifactReleaseExperimentBinding {
            outbox_uid: record.outbox_uid,
            activation_target: attempt.activation_target.as_str().to_string(),
            trials: attempt
                .trials
                .iter()
                .map(|trial| ArtifactReleaseExperimentTrialBinding {
                    trial_key: trial.trial_key.clone(),
                    arm: ArtifactReleaseExperimentArm {
                        variant_key: match trial.role {
                            ArmRole::Candidate => ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
                            ArmRole::Baseline => ARTIFACT_RELEASE_BASELINE_VARIANT_KEY,
                        }
                        .to_string(),
                        revision_uid: trial.revision_uid,
                        overlay_uid: trial.overlay_uid,
                        overlay_token: trial.overlay_token.clone(),
                        eval_session_id: trial.eval_session_id,
                    },
                    case: trial.case.clone(),
                })
                .collect(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::artifact_release_evaluation::types::{
        DispatchStatus, MergedCasePlan, ProvisionedTrial, ReleaseCase,
    };
    use moa_artifacts::release::{ActivationTargetClass, Digest32};
    use moa_eval_core::assertion::{AssertionCategory, AssertionSpec, EvaluatorRef, GateEffect};
    use serde_json::json;
    use uuid::Uuid;

    fn assertion() -> AssertionSpec {
        AssertionSpec {
            id: "no-dangerous-tool".to_string(),
            category: AssertionCategory::Action,
            gate_effect: GateEffect::Blocking,
            evaluator: EvaluatorRef::deterministic("prohibited_actions", 1),
            config: json!({ "names": ["dangerous_tool"] }),
        }
    }

    fn positive_assertion() -> AssertionSpec {
        AssertionSpec {
            id: "visible-result".to_string(),
            category: AssertionCategory::Communication,
            gate_effect: GateEffect::Blocking,
            evaluator: EvaluatorRef::deterministic("text_match", 1),
            config: json!({ "contains": ["result"] }),
        }
    }

    fn trial(role: ArmRole, revision: u128) -> ProvisionedTrial {
        ProvisionedTrial {
            trial_key: format!("trial-{role}"),
            role,
            case: moa_wire::experiments::ArtifactReleaseExperimentCase {
                scenario_id: "a".to_string(),
                persona_id: "persona://p".to_string(),
                profile_id: "default".to_string(),
                repetitions: 1,
                assertions: vec![positive_assertion(), assertion()],
            },
            overlay_uid: Uuid::from_u128(revision + 100),
            overlay_token: "token".to_string(),
            revision_uid: Uuid::from_u128(revision),
            eval_session_id: Uuid::from_u128(revision + 200),
        }
    }

    fn provisioned(trials: Vec<ProvisionedTrial>) -> ProvisionedAttempt {
        ProvisionedAttempt {
            attempt_uid: Uuid::from_u128(1),
            activation_target: ActivationTargetClass::SkillVisibility,
            plan: MergedCasePlan {
                authoring_pack_uid: Uuid::from_u128(2),
                hidden_pack_uid: Uuid::from_u128(3),
                cohort_epoch: 4,
                plan_revision_uid: Uuid::from_u128(5),
                authoring_cases: vec![ReleaseCase {
                    case_id: "a".to_string(),
                    persona_ref: "persona://p".to_string(),
                    profile: "default".to_string(),
                    repetitions: 1,
                    assertions: Vec::new(),
                }],
                hidden_cases: Vec::new(),
                mandatory_assertions: vec![assertion()],
            },
            trials,
        }
    }

    fn record() -> DispatchRecord {
        DispatchRecord {
            outbox_uid: Uuid::from_u128(9),
            tenant_id: TenantId::from(Uuid::from_u128(10)),
            revision_uid: Uuid::from_u128(11),
            artifact_uid: Uuid::from_u128(12),
            generation: 3,
            subject_digest: Digest32([5_u8; 32]),
            idempotency_key: "release:key".to_string(),
            status: DispatchStatus::Dispatched,
            seed_material: "seed".to_string(),
            pinned_dependencies: Vec::new(),
            case_pack_uid: None,
            hidden_pack_uid: None,
            cohort_epoch: None,
            candidate_run_uid: None,
            baseline_run_uid: None,
            attempt_no: 1,
        }
    }

    // Pins: a release dispatch is one plan-backed paired run and carries the
    // fenced idempotency key so a replay re-admits the same run.
    #[test]
    fn paired_dispatch_shares_one_plan_and_the_fenced_idempotency_key_offline() {
        let record = record();
        let attempt = provisioned(vec![
            trial(ArmRole::Candidate, 21),
            trial(ArmRole::Baseline, 22),
        ]);
        let request =
            build_paired_run_request(record.tenant_id, &record, &attempt).expect("paired request");

        assert_eq!(request.plan_revision_uid, Uuid::from_u128(5));
        assert_eq!(request.idempotency_key.as_deref(), Some("release:key"));
        assert!(request.agent_revision_variants.is_empty());
        assert_eq!(
            request
                .release_evaluation
                .as_ref()
                .map(|binding| binding.trials.len()),
            Some(2),
            "both arms must be release variants of the same plan-backed run"
        );
        let binding = request
            .release_evaluation
            .as_ref()
            .expect("release binding");
        assert_eq!(
            binding.trials[0].case.assertions,
            vec![positive_assertion(), assertion()],
            "case-local and mandatory assertions must reach trial expansion"
        );

        // A first activation has no serving-pointer baseline. It runs the
        // candidate-only absolute gate instead of fabricating a control arm.
        let unpaired = provisioned(vec![trial(ArmRole::Candidate, 21)]);
        let request = build_paired_run_request(record.tenant_id, &record, &unpaired)
            .expect("unpaired request");
        assert_eq!(
            request
                .release_evaluation
                .as_ref()
                .map(|binding| binding.trials.len()),
            Some(1)
        );

        // Skill and action releases substitute through the evaluation overlay, not
        // through agent revision variants.
        let request =
            build_paired_run_request(record.tenant_id, &record, &attempt).expect("skill request");
        assert!(request.agent_revision_variants.is_empty());

        // An attempt with no candidate arm has nothing to evaluate.
        assert!(matches!(
            build_paired_run_request(
                record.tenant_id,
                &record,
                &provisioned(vec![trial(ArmRole::Baseline, 22)]),
            ),
            Err(Error::Provisioning(_))
        ));
    }
}
