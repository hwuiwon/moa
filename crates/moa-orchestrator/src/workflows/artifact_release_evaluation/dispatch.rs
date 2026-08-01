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

use moa_artifacts::release::ActivationTargetClass;
use moa_core::types::identifiers::TenantId;
use moa_wire::experiments::{
    ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
    ArtifactReleaseExperimentArm, ArtifactReleaseExperimentBinding, ExperimentRunRequest,
};

use super::Error;
use super::types::{ArmRole, DispatchRecord, ProvisionedAttempt};

/// Builds the paired experiment run request for one release attempt.
///
/// Three properties are asserted by construction rather than checked later:
///
/// * `target` is `None`. A plan-backed run builds its own target during
///   admission, so this request cannot name a caller-owned session, and the
///   sessions the run creates are eval-owned.
/// * `idempotency_key` is the dispatch record's key, which is deterministic in
///   `(revision, generation, subject digest)`. A Restate replay of the same
///   attempt therefore re-admits the same run instead of starting a second one.
/// * Candidate and baseline are release variants of one run, which is what pairs
///   them on identical seeds for every artifact class.
pub fn build_paired_run_request(
    tenant_id: TenantId,
    record: &DispatchRecord,
    activation_target: ActivationTargetClass,
    attempt: &ProvisionedAttempt,
) -> Result<ExperimentRunRequest, Error> {
    if attempt.arm(ArmRole::Candidate).is_none() {
        return Err(Error::Provisioning(
            "a release attempt has no candidate arm to run".to_string(),
        ));
    }
    Ok(ExperimentRunRequest {
        tenant_id,
        name: format!(
            "release:{}:{}:{}",
            activation_target, record.revision_uid, record.generation
        ),
        plan_revision_uid: Some(attempt.plan.plan_revision_uid),
        // Deliberately absent. A plan-backed run derives its own target, so this
        // request has no way to name a session the caller already owns.
        target: None,
        variant: None,
        // Also absent: a plan-backed run takes its scorecard from the pinned plan
        // revision, so a submitter cannot declare weaker evidence requirements.
        scorecard: None,
        score_run_id: None,
        idempotency_key: Some(record.idempotency_key.clone()),
        agent_revision_variants: Vec::new(),
        release_evaluation: Some(ArtifactReleaseExperimentBinding {
            outbox_uid: record.outbox_uid,
            activation_target: activation_target.as_str().to_string(),
            arms: attempt
                .arms
                .iter()
                .map(|arm| ArtifactReleaseExperimentArm {
                    variant_key: match arm.role {
                        ArmRole::Candidate => ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
                        ArmRole::Baseline => ARTIFACT_RELEASE_BASELINE_VARIANT_KEY,
                    }
                    .to_string(),
                    revision_uid: arm.revision_uid,
                    overlay_uid: arm.overlay_uid,
                    overlay_token: arm.overlay_token.clone(),
                    eval_session_id: arm.eval_session_id,
                })
                .collect(),
            cases: attempt.plan.experiment_cases()?,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::artifact_release_evaluation::types::{
        DispatchStatus, MergedCasePlan, ProvisionedArm, ReleaseCase,
    };
    use moa_artifacts::release::Digest32;
    use uuid::Uuid;

    fn arm(role: ArmRole, revision: u128) -> ProvisionedArm {
        ProvisionedArm {
            role,
            overlay_uid: Uuid::from_u128(revision + 100),
            overlay_token: "token".to_string(),
            revision_uid: Uuid::from_u128(revision),
            eval_session_id: Uuid::from_u128(revision + 200),
            fixture_uid: Uuid::from_u128(revision + 300),
        }
    }

    fn provisioned(arms: Vec<ProvisionedArm>) -> ProvisionedAttempt {
        ProvisionedAttempt {
            attempt_uid: Uuid::from_u128(1),
            plan: MergedCasePlan {
                authoring_pack_uid: Uuid::from_u128(2),
                hidden_pack_uid: Uuid::from_u128(3),
                cohort_epoch: 4,
                plan_revision_uid: Uuid::from_u128(5),
                authoring_cases: vec![ReleaseCase {
                    case_id: "a".to_string(),
                    persona_ref: "persona://p".to_string(),
                    profile: "default".to_string(),
                    repetitions: 2,
                    assertions: Vec::new(),
                }],
                hidden_cases: Vec::new(),
                mandatory_assertions: vec!["privacy_safe_output".to_string()],
            },
            arms,
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

    // Pins: a release dispatch is one plan-backed paired run that names no
    // caller-owned session, declares no scorecard of its own, and carries the
    // fenced idempotency key so a replay re-admits the same run.
    #[test]
    fn paired_dispatch_shares_one_plan_and_the_fenced_idempotency_key_offline() {
        let record = record();
        let attempt = provisioned(vec![
            arm(ArmRole::Candidate, 21),
            arm(ArmRole::Baseline, 22),
        ]);
        let request = build_paired_run_request(
            record.tenant_id,
            &record,
            ActivationTargetClass::AgentDeployment,
            &attempt,
        )
        .expect("paired request");

        assert_eq!(request.plan_revision_uid, Some(Uuid::from_u128(5)));
        assert!(
            request.target.is_none(),
            "a release run must not be able to name a caller-owned session"
        );
        assert!(request.scorecard.is_none());
        assert_eq!(request.idempotency_key.as_deref(), Some("release:key"));
        assert!(request.agent_revision_variants.is_empty());
        assert_eq!(
            request
                .release_evaluation
                .as_ref()
                .map(|binding| binding.arms.len()),
            Some(2),
            "both arms must be release variants of the same plan-backed run"
        );
        assert_eq!(
            request
                .release_evaluation
                .as_ref()
                .map(|binding| binding.cases.len()),
            Some(1)
        );

        // A first activation has no serving-pointer overlay. The binding carries
        // only the candidate; plan expansion adds the approved plan's authored
        // target as `release_baseline`, preserving a real control arm without
        // pretending that an older artifact revision exists.
        let unpaired = provisioned(vec![arm(ArmRole::Candidate, 21)]);
        let request = build_paired_run_request(
            record.tenant_id,
            &record,
            ActivationTargetClass::AgentDeployment,
            &unpaired,
        )
        .expect("unpaired request");
        assert_eq!(
            request
                .release_evaluation
                .as_ref()
                .map(|binding| binding.arms.len()),
            Some(1)
        );

        // Skill and action releases substitute through the evaluation overlay, not
        // through agent revision variants.
        let request = build_paired_run_request(
            record.tenant_id,
            &record,
            ActivationTargetClass::SkillVisibility,
            &attempt,
        )
        .expect("skill request");
        assert!(request.agent_revision_variants.is_empty());

        // An attempt with no candidate arm has nothing to evaluate.
        assert!(matches!(
            build_paired_run_request(
                record.tenant_id,
                &record,
                ActivationTargetClass::SkillVisibility,
                &provisioned(vec![arm(ArmRole::Baseline, 22)]),
            ),
            Err(Error::Provisioning(_))
        ));
    }
}
