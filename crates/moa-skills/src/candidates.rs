//! Learning-candidate helpers for skill creation and improvement proposals.

use chrono::{DateTime, Utc};
use moa_core::{
    types::experience::LearningCandidate, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateType, types::experience::LearningRiskClass,
    types::experience::TaskFacetSet, types::experience::TaskFingerprint,
    types::identifiers::SessionId, types::identifiers::TenantId, types::memory::SkillMetadata,
    types::session::SessionMeta,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) fn deterministic_skill_candidate_id(
    tenant_id: TenantId,
    source_session_id: SessionId,
    source_experience_ids: &[Uuid],
    operation: &str,
    target_skill_name: &str,
) -> Uuid {
    let mut source_experience_ids = source_experience_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>();
    source_experience_ids.sort();

    let mut hasher = Sha256::new();
    for part in [
        "moa.skill.learning_candidate.v1",
        &tenant_id.to_string(),
        operation,
        target_skill_name,
    ] {
        hash_part(&mut hasher, part);
    }
    if source_experience_ids.is_empty() {
        hash_part(&mut hasher, "source_session_id");
        hash_part(&mut hasher, &source_session_id.to_string());
    } else {
        hash_part(&mut hasher, "source_experience_ids");
        for source_experience_id in source_experience_ids {
            hash_part(&mut hasher, &source_experience_id);
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
    pub source_experience_ids: Vec<Uuid>,
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
        status: LearningCandidateStatus::Proposed,
        target_id: Some(input.metadata.path),
        target_label: Some(input.metadata.name),
        task_fingerprint: input.task_fingerprint,
        task_facets: input.task_facets,
        payload: input.payload,
        evaluation_payload: None,
        source_experience_ids: input.source_experience_ids,
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
