//! Learning-candidate helpers for skill creation and improvement proposals.

use chrono::{DateTime, Utc};
use moa_core::{
    types::experience::LearningCandidate, types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateType, types::experience::LearningProposalKind,
    types::experience::LearningRiskClass, types::experience::TaskFacetSet,
    types::experience::TaskFingerprint, types::identifiers::SessionId,
    types::identifiers::TenantId, types::memory::SkillMetadata, types::session::SessionMeta,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) fn deterministic_skill_candidate_id(
    tenant_id: TenantId,
    source_session_id: SessionId,
    experience_ids: &[Uuid],
    operation: &str,
    target_skill_name: &str,
) -> Uuid {
    let mut experience_ids = experience_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>();
    experience_ids.sort();

    let mut hasher = Sha256::new();
    for part in [
        "moa.skill.learning_candidate.v1",
        &tenant_id.to_string(),
        operation,
        target_skill_name,
    ] {
        hash_part(&mut hasher, part);
    }
    if experience_ids.is_empty() {
        hash_part(&mut hasher, "source_session_id");
        hash_part(&mut hasher, &source_session_id.to_string());
    } else {
        hash_part(&mut hasher, "source_experiences");
        for experience_id in experience_ids {
            hash_part(&mut hasher, &experience_id);
        }
    }

    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

pub(crate) struct SkillDraftCandidateInput {
    pub candidate_id: Uuid,
    pub operation: String,
    pub metadata: SkillMetadata,
    pub payload: Value,
    pub sources: Vec<LearningCandidateSourceRef>,
    pub task_fingerprint: Option<TaskFingerprint>,
    pub task_facets: Option<TaskFacetSet>,
    pub confidence: Option<f64>,
    pub now: DateTime<Utc>,
}

pub(crate) fn skill_draft_candidate(
    session: &SessionMeta,
    input: SkillDraftCandidateInput,
) -> LearningCandidate {
    LearningCandidate {
        id: input.candidate_id,
        tenant_id: session.tenant_id,
        user_id: None,
        candidate_type: LearningCandidateType::Skill,
        proposal_kind: LearningProposalKind::SkillDraft,
        status: LearningProposalKind::SkillDraft.initial_status(),
        target_id: Some(input.metadata.path),
        target_label: Some(input.metadata.name),
        task_fingerprint: input.task_fingerprint,
        task_facets: input.task_facets,
        payload: input.payload,
        evaluation_payload: None,
        sources: input.sources,
        confidence: input.confidence,
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec![
            "human_review".to_string(),
            "artifact_draft_review".to_string(),
            "regression_suite_review".to_string(),
        ],
        status_reason: Some(format!(
            "{} draft generated; waiting for review",
            input.operation
        )),
        batch_id: None,
        created_at: input.now,
        updated_at: input.now,
    }
}

/// Returns the experience ids named by a typed source list, in given order.
///
/// The candidate id is keyed on evidence identity so retries of the same
/// proposal dedupe, and only experience references carry that identity.
pub(crate) fn experience_ids(sources: &[LearningCandidateSourceRef]) -> Vec<Uuid> {
    sources
        .iter()
        .filter_map(|source| match source {
            LearningCandidateSourceRef::Experience { experience_id } => Some(*experience_id),
            _ => None,
        })
        .collect()
}

fn hash_part(hasher: &mut Sha256, part: &str) {
    hasher.update(part.len().to_le_bytes());
    hasher.update(part.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        TenantId::from(Uuid::from_u128(7))
    }

    #[test]
    fn deterministic_candidate_id_is_stable_for_identical_inputs() {
        // Pins: the candidate id is a pure function of its inputs, so retries of the same
        // proposal dedupe to one learning candidate instead of creating duplicates.
        let session = SessionId(Uuid::from_u128(9));
        let first =
            deterministic_skill_candidate_id(tenant(), session, &[], "skill_created", "auth-flow");
        let second =
            deterministic_skill_candidate_id(tenant(), session, &[], "skill_created", "auth-flow");

        assert_eq!(first, second);
    }

    #[test]
    fn deterministic_candidate_id_differs_on_operation_name_or_session() {
        // Pins: changing the operation, target name, or source session yields a distinct id.
        let session = SessionId(Uuid::from_u128(9));
        let base =
            deterministic_skill_candidate_id(tenant(), session, &[], "skill_created", "auth-flow");
        let other_operation =
            deterministic_skill_candidate_id(tenant(), session, &[], "skill_improved", "auth-flow");
        let other_name = deterministic_skill_candidate_id(
            tenant(),
            session,
            &[],
            "skill_created",
            "deploy-flow",
        );
        let other_session = deterministic_skill_candidate_id(
            tenant(),
            SessionId(Uuid::from_u128(10)),
            &[],
            "skill_created",
            "auth-flow",
        );

        assert_ne!(base, other_operation);
        assert_ne!(base, other_name);
        assert_ne!(base, other_session);
    }

    #[test]
    fn deterministic_candidate_id_keys_on_sorted_experience_ids_when_present() {
        // Pins: when source experience ids exist they key the id order-independently and
        // supersede the session, so the same evidence dedupes across sessions and orderings.
        let exp1 = Uuid::from_u128(100);
        let exp2 = Uuid::from_u128(200);
        let from_session_a = deterministic_skill_candidate_id(
            tenant(),
            SessionId(Uuid::from_u128(1)),
            &[exp1, exp2],
            "skill_created",
            "auth-flow",
        );
        let from_session_b_reordered = deterministic_skill_candidate_id(
            tenant(),
            SessionId(Uuid::from_u128(2)),
            &[exp2, exp1],
            "skill_created",
            "auth-flow",
        );

        assert_eq!(from_session_a, from_session_b_reordered);
    }
}
