//! Learning-candidate helpers for skill creation and improvement.

use chrono::{DateTime, Utc};
use moa_core::{
    ExperienceRecord, LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningRiskClass, SessionMeta, SkillMetadata,
};
use serde_json::json;
use uuid::Uuid;

/// Builds a proposed skill-creation candidate from generated skill markdown.
#[must_use]
pub fn skill_creation_candidate(
    session: &SessionMeta,
    experience: &ExperienceRecord,
    metadata: &SkillMetadata,
    skill_markdown: &str,
    now: DateTime<Utc>,
) -> LearningCandidate {
    LearningCandidate {
        id: Uuid::now_v7(),
        tenant_id: session.workspace_id.to_string(),
        workspace_id: session.workspace_id.clone(),
        user_id: None,
        candidate_type: LearningCandidateType::Skill,
        status: LearningCandidateStatus::Proposed,
        target_id: Some(metadata.path.clone()),
        target_label: Some(metadata.name.clone()),
        task_fingerprint: Some(experience.task_fingerprint.clone()),
        task_facets: Some(experience.task_facets.clone()),
        payload: json!({
            "operation": "skill_created",
            "name": metadata.name,
            "path": metadata.path,
            "description": metadata.description,
            "skill_markdown": skill_markdown,
            "expected_task_fingerprint": experience.task_fingerprint.hash,
        }),
        evaluation_payload: None,
        source_experience_ids: vec![experience.id],
        confidence: Some(experience.confidence),
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec![
            "skill_markdown_parse".to_string(),
            "regression_suite_generation".to_string(),
        ],
        status_reason: None,
        batch_id: None,
        created_at: now,
        updated_at: now,
    }
}

/// Builds a proposed skill-improvement candidate from generated skill markdown.
#[must_use]
pub fn skill_improvement_candidate(
    session: &SessionMeta,
    experience: &ExperienceRecord,
    metadata: &SkillMetadata,
    previous_version: &str,
    skill_markdown: &str,
    now: DateTime<Utc>,
) -> LearningCandidate {
    let mut candidate =
        skill_creation_candidate(session, experience, metadata, skill_markdown, now);
    candidate.payload = json!({
        "operation": "skill_improved",
        "name": metadata.name,
        "path": metadata.path,
        "previous_version": previous_version,
        "skill_markdown": skill_markdown,
        "expected_task_fingerprint": experience.task_fingerprint.hash,
    });
    candidate.promotion_requirements = vec![
        "skill_markdown_parse".to_string(),
        "regression_comparison".to_string(),
    ];
    candidate
}

/// Builds a candidate status update.
#[must_use]
pub fn candidate_status_update(
    candidate_id: Uuid,
    status: LearningCandidateStatus,
    reason: impl Into<String>,
    evaluation_payload: Option<serde_json::Value>,
    now: DateTime<Utc>,
) -> LearningCandidateStatusUpdate {
    LearningCandidateStatusUpdate {
        candidate_id,
        status,
        status_reason: Some(reason.into()),
        evaluation_payload,
        updated_at: now,
    }
}
