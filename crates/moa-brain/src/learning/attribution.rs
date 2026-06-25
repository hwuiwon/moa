//! Deterministic attribution generation for experience records.

use chrono::{DateTime, Utc};
use moa_core::{
    AttributionEffect, AttributionSubjectType, Event, EventRecord, ExperienceAttribution,
    ExperienceRecord, SegmentEvidenceKind, SegmentEvidencePolarity, SegmentOutcome, UserId,
};
use uuid::Uuid;

/// Generates deterministic attributions for the subjects visible in an experience.
#[must_use]
pub fn attributions_for_experience(
    experience: &ExperienceRecord,
    events: &[EventRecord],
    now: DateTime<Utc>,
) -> Vec<ExperienceAttribution> {
    let mut attributions = Vec::new();
    for skill in &experience.skills_activated {
        attributions.push(attribution(
            experience,
            AttributionSubjectType::Skill,
            skill,
            outcome_effect(experience.outcome),
            experience.confidence,
            vec![format!(
                "skill `{skill}` was active during assessed segment"
            )],
            now,
        ));
    }
    for tool in &experience.tools_used {
        attributions.push(attribution(
            experience,
            AttributionSubjectType::Tool,
            tool,
            tool_effect(tool, experience.outcome, events),
            tool_confidence(tool, experience.confidence, events),
            vec![format!("tool `{tool}` was used during assessed segment")],
            now,
        ));
    }
    for record in events {
        match &record.event {
            Event::MemoryRead { path, .. } | Event::MemoryWrite { path, .. } => {
                attributions.push(attribution(
                    experience,
                    AttributionSubjectType::Memory,
                    path,
                    outcome_effect(experience.outcome),
                    (experience.confidence * 0.8).clamp(0.0, 1.0),
                    vec![format!("memory `{path}` was touched during the segment")],
                    now,
                ));
            }
            Event::MemoryIngest { source_path, .. } => {
                attributions.push(attribution(
                    experience,
                    AttributionSubjectType::Memory,
                    source_path,
                    outcome_effect(experience.outcome),
                    (experience.confidence * 0.8).clamp(0.0, 1.0),
                    vec![format!(
                        "memory source `{source_path}` was ingested during the segment"
                    )],
                    now,
                ));
            }
            _ => {}
        }
    }
    if let Some(summary) = verification_evidence(experience) {
        attributions.push(attribution(
            experience,
            AttributionSubjectType::Verification,
            "verification",
            outcome_effect(experience.outcome),
            experience.confidence,
            vec![summary],
            now,
        ));
    }
    attributions.sort_by(|left, right| {
        left.subject_type
            .as_str()
            .cmp(right.subject_type.as_str())
            .then_with(|| left.subject_id.cmp(&right.subject_id))
    });
    attributions.dedup_by(|left, right| {
        left.subject_type == right.subject_type && left.subject_id == right.subject_id
    });
    attributions
}

fn attribution(
    experience: &ExperienceRecord,
    subject_type: AttributionSubjectType,
    subject_id: &str,
    effect: AttributionEffect,
    confidence: f64,
    evidence: Vec<String>,
    now: DateTime<Utc>,
) -> ExperienceAttribution {
    ExperienceAttribution {
        id: Uuid::now_v7(),
        experience_id: experience.id,
        tenant_id: experience.tenant_id,
        user_id: Some(UserId(experience.user_id.to_string())),
        subject_type,
        subject_id: subject_id.to_string(),
        effect,
        confidence: confidence.clamp(0.0, 1.0),
        evidence,
        created_at: now,
    }
}

fn outcome_effect(outcome: SegmentOutcome) -> AttributionEffect {
    match outcome {
        SegmentOutcome::Resolved => AttributionEffect::Helpful,
        SegmentOutcome::Partial => AttributionEffect::Mixed,
        SegmentOutcome::Unknown => AttributionEffect::Neutral,
        SegmentOutcome::Failed | SegmentOutcome::Abandoned => AttributionEffect::Harmful,
        _ => AttributionEffect::Neutral,
    }
}

fn tool_effect(tool: &str, outcome: SegmentOutcome, events: &[EventRecord]) -> AttributionEffect {
    let had_error = events.iter().any(
        |record| matches!(&record.event, Event::ToolError { tool_name, .. } if tool_name == tool),
    );
    if had_error {
        return AttributionEffect::Harmful;
    }
    outcome_effect(outcome)
}

fn tool_confidence(tool: &str, base: f64, events: &[EventRecord]) -> f64 {
    let observed_result = events.iter().any(|record| match &record.event {
        Event::ToolCall { tool_name, .. } | Event::ToolError { tool_name, .. } => tool_name == tool,
        _ => false,
    });
    if observed_result { base } else { base * 0.7 }
}

fn verification_evidence(experience: &ExperienceRecord) -> Option<String> {
    experience
        .evidence
        .iter()
        .find(|evidence| evidence.kind == SegmentEvidenceKind::Verification)
        .map(|evidence| match evidence.polarity {
            SegmentEvidencePolarity::SupportsResolved => {
                format!("verification supported success: {}", evidence.summary)
            }
            SegmentEvidencePolarity::SupportsFailed => {
                format!("verification supported failure: {}", evidence.summary)
            }
            _ => format!("verification evidence: {}", evidence.summary),
        })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_core::{
        SegmentEvidence, SegmentEvidenceKind, SegmentEvidencePolarity, SegmentId, SessionId,
        TaskFacetSet, TaskFingerprint, TenantId,
    };

    use super::*;

    #[test]
    fn attribution_separates_skill_tool_memory_and_verification() {
        // Pins: attribution is separate from assessment and emits deterministic subject rows.
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
            task_summary: Some("Fix tests".to_string()),
            task_fingerprint: TaskFingerprint {
                hash: "hash".to_string(),
                normalized_summary: "fix tests".to_string(),
                policy_version: "v1".to_string(),
            },
            task_facets: TaskFacetSet::default(),
            actions: Vec::new(),
            resources: Vec::new(),
            outcome: SegmentOutcome::Resolved,
            confidence: 0.8,
            evidence: vec![SegmentEvidence {
                kind: SegmentEvidenceKind::Verification,
                polarity: SegmentEvidencePolarity::SupportsResolved,
                strength: 0.9,
                summary: "cargo test passed".to_string(),
            }],
            tools_used: vec!["bash".to_string()],
            skills_activated: vec!["rust".to_string()],
            turn_count: 2,
            token_cost: 10,
            duration_ms: Some(50),
            assessment_policy_version: "assessment_v1".to_string(),
            extraction_policy_version: "experience_v1".to_string(),
            created_at: now,
        };
        let events = vec![EventRecord {
            id: Uuid::now_v7(),
            session_id: experience.session_id,
            sequence_num: 0,
            event_type: moa_core::EventType::MemoryRead,
            event: Event::MemoryRead {
                path: "docs/architecture".to_string(),
                scope: "tenant".to_string(),
            },
            timestamp: now,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }];

        let attributions = attributions_for_experience(&experience, &events, now);
        let subjects = attributions
            .iter()
            .map(|row| {
                (
                    row.subject_type.as_str(),
                    row.subject_id.as_str(),
                    row.effect,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            subjects,
            vec![
                ("memory", "docs/architecture", AttributionEffect::Helpful),
                ("skill", "rust", AttributionEffect::Helpful),
                ("tool", "bash", AttributionEffect::Helpful),
                ("verification", "verification", AttributionEffect::Helpful),
            ]
        );
    }
}
