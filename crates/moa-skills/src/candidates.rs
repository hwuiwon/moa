//! Learning-candidate helpers for skill creation and improvement proposals.

use chrono::{DateTime, Utc};
use moa_core::{
    LearningCandidate, LearningCandidateStatus, LearningCandidateType, LearningRiskClass,
    SessionId, SessionMeta, SkillMetadata, TaskFacetSet, TaskFingerprint, WorkspaceId,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) fn deterministic_skill_candidate_id(
    workspace_id: &WorkspaceId,
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
        workspace_id.as_str(),
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
        tenant_id: session.workspace_id.to_string(),
        workspace_id: session.workspace_id.clone(),
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
