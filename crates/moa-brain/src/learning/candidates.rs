//! Learning-candidate proposal helpers for non-skill self-improvement outputs.

use chrono::{DateTime, Utc};
use moa_core::{
    types::experience::AttributionEffect, types::experience::AttributionSubjectType,
    types::experience::ExperienceAttribution, types::experience::ExperienceRecord,
    types::experience::LearningCandidate, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateType, types::experience::LearningRiskClass,
    types::segment_assessment::SegmentOutcome,
};
use serde_json::json;
use uuid::Uuid;

/// Proposes memory, policy, and eval candidates without promoting them.
#[must_use]
pub fn propose_candidates_for_experience(
    experience: &ExperienceRecord,
    attributions: &[ExperienceAttribution],
    now: DateTime<Utc>,
) -> Vec<LearningCandidate> {
    let mut candidates = Vec::new();
    if should_propose_memory(experience, attributions) {
        candidates.push(base_candidate(
            experience,
            LearningCandidateType::Memory,
            "workspace lesson",
            json!({
                "task_summary": experience.task_summary,
                "outcome": experience.outcome.as_str(),
                "facets": experience.task_facets,
                "lesson": "candidate requires consolidation review before memory write"
            }),
            LearningRiskClass::Low,
            vec!["memory_consolidation_review".to_string()],
            now,
        ));
    }
    if should_propose_eval(experience, attributions) {
        candidates.push(base_candidate(
            experience,
            LearningCandidateType::Eval,
            "regression scenario",
            json!({
                "task_summary": experience.task_summary,
                "outcome": experience.outcome.as_str(),
                "evidence": experience.evidence,
                "bounded_reproduction": {
                    "tools_used": experience.tools_used,
                    "skills_activated": experience.skills_activated
                }
            }),
            LearningRiskClass::Low,
            vec![
                "human_review".to_string(),
                "scenario_minimization".to_string(),
            ],
            now,
        ));
    }
    if should_propose_policy(experience, attributions) {
        candidates.push(base_candidate(
            experience,
            LearningCandidateType::Policy,
            "verification policy pattern",
            json!({
                "task_summary": experience.task_summary,
                "facets": experience.task_facets,
                "pattern": "repeated verification behavior candidate; promotion requires repeated evidence"
            }),
            LearningRiskClass::Medium,
            vec!["repeated_evidence".to_string(), "human_approval".to_string()],
            now,
        ));
    }
    candidates
}

fn base_candidate(
    experience: &ExperienceRecord,
    candidate_type: LearningCandidateType,
    label: &str,
    payload: serde_json::Value,
    risk_class: LearningRiskClass,
    promotion_requirements: Vec<String>,
    now: DateTime<Utc>,
) -> LearningCandidate {
    LearningCandidate {
        id: Uuid::now_v7(),
        tenant_id: experience.tenant_id,
        user_id: Some(experience.user_id.clone()),
        candidate_type,
        status: LearningCandidateStatus::Proposed,
        target_id: None,
        target_label: Some(label.to_string()),
        task_fingerprint: Some(experience.task_fingerprint.clone()),
        task_facets: Some(experience.task_facets.clone()),
        payload,
        evaluation_payload: None,
        source_experience_ids: vec![experience.id],
        confidence: Some(experience.confidence),
        risk_class,
        promotion_requirements,
        status_reason: None,
        batch_id: None,
        created_at: now,
        updated_at: now,
    }
}

fn should_propose_memory(
    experience: &ExperienceRecord,
    attributions: &[ExperienceAttribution],
) -> bool {
    matches!(experience.outcome, SegmentOutcome::Resolved)
        && experience.confidence >= 0.75
        && attributions
            .iter()
            .any(|row| row.effect == AttributionEffect::Helpful)
}

fn should_propose_eval(
    experience: &ExperienceRecord,
    attributions: &[ExperienceAttribution],
) -> bool {
    matches!(
        experience.outcome,
        SegmentOutcome::Failed | SegmentOutcome::Abandoned
    ) && experience.confidence >= 0.6
        && attributions
            .iter()
            .any(|row| row.effect == AttributionEffect::Harmful)
}

fn should_propose_policy(
    experience: &ExperienceRecord,
    attributions: &[ExperienceAttribution],
) -> bool {
    experience.turn_count >= 3
        && attributions
            .iter()
            .any(|row| row.subject_type == AttributionSubjectType::Verification)
        && experience
            .task_facets
            .verification_style
            .as_deref()
            .is_some_and(|style| style == "command")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_core::{
        types::experience::TaskFacetSet, types::experience::TaskFingerprint,
        types::identifiers::SegmentId, types::identifiers::SessionId, types::identifiers::TenantId,
        types::identifiers::UserId,
    };

    use super::*;

    #[test]
    fn failed_experience_proposes_eval_not_promotion() {
        // Pins: high-confidence failures become eval candidates that stay proposed.
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        let experience = ExperienceRecord {
            id: Uuid::now_v7(),
            segment_id: SegmentId::new(),
            session_id: SessionId::new(),
            tenant_id: TenantId::new(),
            user_id: UserId::new("user"),
            task_summary: Some("Fix deploy".to_string()),
            task_fingerprint: TaskFingerprint {
                hash: "hash".to_string(),
                normalized_summary: "deploy fix".to_string(),
                policy_version: "experience_v1".to_string(),
            },
            task_facets: TaskFacetSet::default(),
            actions: Vec::new(),
            resources: Vec::new(),
            outcome: SegmentOutcome::Failed,
            confidence: 0.8,
            evidence: Vec::new(),
            tools_used: vec!["bash".to_string()],
            skills_activated: Vec::new(),
            turn_count: 1,
            token_cost: 10,
            duration_ms: None,
            assessment_policy_version: "assessment_v1".to_string(),
            extraction_policy_version: "experience_v1".to_string(),
            created_at: now,
        };
        let attribution = ExperienceAttribution {
            id: Uuid::now_v7(),
            experience_id: experience.id,
            tenant_id: TenantId::new(),
            user_id: None,
            subject_type: AttributionSubjectType::Tool,
            subject_id: "bash".to_string(),
            effect: AttributionEffect::Harmful,
            confidence: 0.8,
            evidence: Vec::new(),
            created_at: now,
        };

        let candidates = propose_candidates_for_experience(&experience, &[attribution], now);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].candidate_type, LearningCandidateType::Eval);
        assert_eq!(candidates[0].status, LearningCandidateStatus::Proposed);
    }
}
