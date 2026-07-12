//! Deterministic attribution generation for experience records.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_core::{
    events::Event, types::events_stream::EventRecord, types::experience::AttributionEffect,
    types::experience::AttributionKind, types::experience::AttributionSubjectType,
    types::experience::ExperienceAttribution, types::experience::ExperienceRecord,
    types::identifiers::UserId, types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity, types::segment_assessment::SegmentOutcome,
    types::skill_use::skills_used_in_tool_call,
};
use uuid::Uuid;

/// Generates deterministic attributions for the subjects visible in an experience.
///
/// Skills that the model actually engaged ([`ExperienceRecord::skills_used`]) are
/// credited or blamed by the segment outcome. Skills that were injected into the
/// turn manifest but never engaged are recorded as
/// [`AttributionKind::UnusedInjection`] with a `Neutral` effect: they are weak
/// negative-relevance evidence, not evidence that the skill helped or hurt, so
/// ranking excludes them from success rates.
///
/// A used skill's effect is otherwise a pure function of the segment outcome,
/// which would make it a mathematical duplicate of the outcome-derived success
/// rate. To give skill effects independent signal, a used skill whose own
/// engaging tool call ended in a durable error is downgraded one step
/// (Helpful→Mixed, Mixed→Harmful); see [`skills_with_failed_engagement`].
#[must_use]
pub fn attributions_for_experience(
    experience: &ExperienceRecord,
    events: &[EventRecord],
    now: DateTime<Utc>,
) -> Vec<ExperienceAttribution> {
    let mut attributions = Vec::new();
    let failed_skills = skills_with_failed_engagement(&experience.skills_used, events);
    for skill in &experience.skills_activated {
        if experience.skills_used.iter().any(|used| used == skill) {
            let base_effect = outcome_effect(experience.outcome);
            let (effect, evidence) = if failed_skills.contains(skill) {
                (
                    downgrade_effect(base_effect),
                    format!(
                        "skill `{skill}` was engaged during assessed segment; its tool call \
                         failed, so its effect was downgraded"
                    ),
                )
            } else {
                (
                    base_effect,
                    format!("skill `{skill}` was engaged during assessed segment"),
                )
            };
            attributions.push(attribution(
                experience,
                AttributionSubjectType::Skill,
                skill,
                effect,
                AttributionKind::Standard,
                experience.confidence,
                vec![evidence],
                now,
            ));
        } else {
            attributions.push(attribution(
                experience,
                AttributionSubjectType::Skill,
                skill,
                AttributionEffect::Neutral,
                AttributionKind::UnusedInjection,
                experience.confidence,
                vec![format!(
                    "skill `{skill}` was injected but never engaged during assessed segment"
                )],
                now,
            ));
        }
    }
    for tool in &experience.tools_used {
        attributions.push(attribution(
            experience,
            AttributionSubjectType::Tool,
            tool,
            tool_effect(tool, experience.outcome, events),
            AttributionKind::Standard,
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
                    AttributionKind::Standard,
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
                    AttributionKind::Standard,
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
            AttributionKind::Standard,
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

#[allow(clippy::too_many_arguments)]
fn attribution(
    experience: &ExperienceRecord,
    subject_type: AttributionSubjectType,
    subject_id: &str,
    effect: AttributionEffect,
    kind: AttributionKind,
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
        kind,
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

/// Downgrades an attribution effect one step toward `Harmful`.
///
/// `Helpful` becomes `Mixed` and `Mixed` becomes `Harmful`; `Neutral` and
/// `Harmful` are already at or below the mixed floor and are left unchanged.
fn downgrade_effect(effect: AttributionEffect) -> AttributionEffect {
    match effect {
        AttributionEffect::Helpful => AttributionEffect::Mixed,
        AttributionEffect::Mixed => AttributionEffect::Harmful,
        other => other,
    }
}

/// Returns the used skills whose engaging tool call ended in a durable error.
///
/// This is the one place a per-skill signal enters skill attribution. For every
/// durable [`Event::ToolError`] in the segment, the matching [`Event::ToolCall`]
/// is resolved by `tool_id` and its input is run through the same
/// [`skills_used_in_tool_call`] detection that produced `skills_used`; any used
/// skill that the failing call engaged is returned. The rule is deterministic
/// and replay-stable: no proximity heuristic or scoring, only exact tool-id
/// matching. A `ToolError` whose originating call is not present in `events`
/// (for example a truncated event range) attributes to no skill, so the rule
/// never blames a skill it cannot tie to a concrete failed call.
fn skills_with_failed_engagement(
    skills_used: &[String],
    events: &[EventRecord],
) -> HashSet<String> {
    if skills_used.is_empty() {
        return HashSet::new();
    }

    let mut call_inputs = HashMap::new();
    for record in events {
        if let Event::ToolCall {
            tool_id,
            tool_name,
            input,
            ..
        } = &record.event
        {
            call_inputs.insert(*tool_id, (tool_name.as_str(), input));
        }
    }

    let mut failed = HashSet::new();
    for record in events {
        if let Event::ToolError { tool_id, .. } = &record.event
            && let Some((tool_name, input)) = call_inputs.get(tool_id)
        {
            for skill in skills_used_in_tool_call(tool_name, input, skills_used) {
                failed.insert(skill);
            }
        }
    }
    failed
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
        types::experience::TaskFacetSet, types::experience::TaskFingerprint,
        types::identifiers::SegmentId, types::identifiers::SessionId, types::identifiers::TenantId,
        types::segment_assessment::SegmentEvidence, types::segment_assessment::SegmentEvidenceKind,
        types::segment_assessment::SegmentEvidencePolarity,
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
            skills_used: vec!["rust".to_string()],
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
            event_type: moa_core::events::EventType::MemoryRead,
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
        // The engaged skill is credited by outcome, not marked as an unused injection.
        let skill_row = attributions
            .iter()
            .find(|row| row.subject_type == AttributionSubjectType::Skill)
            .expect("engaged skill attribution");
        assert_eq!(skill_row.kind, AttributionKind::Standard);
    }

    #[test]
    fn injected_but_unused_skill_is_neutral_unused_injection() {
        // Pins: a skill injected into the manifest but never engaged is a weak
        // negative-relevance marker (Neutral + UnusedInjection), not credited by outcome.
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
            evidence: Vec::new(),
            tools_used: Vec::new(),
            // Two skills injected; only `used-skill` was engaged.
            skills_activated: vec!["used-skill".to_string(), "unused-skill".to_string()],
            skills_used: vec!["used-skill".to_string()],
            turn_count: 2,
            token_cost: 10,
            duration_ms: Some(50),
            assessment_policy_version: "assessment_v1".to_string(),
            extraction_policy_version: "experience_v1".to_string(),
            created_at: now,
        };

        let attributions = attributions_for_experience(&experience, &[], now);
        let used = attributions
            .iter()
            .find(|row| row.subject_id == "used-skill")
            .expect("used skill attribution");
        let unused = attributions
            .iter()
            .find(|row| row.subject_id == "unused-skill")
            .expect("unused skill attribution");

        assert_eq!(used.effect, AttributionEffect::Helpful);
        assert_eq!(used.kind, AttributionKind::Standard);
        assert_eq!(unused.effect, AttributionEffect::Neutral);
        assert_eq!(unused.kind, AttributionKind::UnusedInjection);
    }

    /// Builds a resolved experience whose only used skill is `skill`.
    fn resolved_experience_using(skill: &str, now: DateTime<Utc>) -> ExperienceRecord {
        ExperienceRecord {
            id: Uuid::now_v7(),
            segment_id: SegmentId::new(),
            session_id: SessionId::new(),
            tenant_id: TenantId::new(),
            user_id: UserId::new("user"),
            task_summary: Some("Export data".to_string()),
            task_fingerprint: TaskFingerprint {
                hash: "hash".to_string(),
                normalized_summary: "export data".to_string(),
                policy_version: "v1".to_string(),
            },
            task_facets: TaskFacetSet::default(),
            actions: Vec::new(),
            resources: Vec::new(),
            outcome: SegmentOutcome::Resolved,
            confidence: 0.8,
            evidence: Vec::new(),
            tools_used: vec!["bash".to_string()],
            skills_activated: vec![skill.to_string()],
            skills_used: vec![skill.to_string()],
            turn_count: 2,
            token_cost: 10,
            duration_ms: Some(50),
            assessment_policy_version: "assessment_v1".to_string(),
            extraction_policy_version: "experience_v1".to_string(),
            created_at: now,
        }
    }

    fn tool_call_event(
        session_id: SessionId,
        tool_id: moa_core::types::identifiers::ToolCallId,
        input: serde_json::Value,
        now: DateTime<Utc>,
    ) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num: 0,
            event_type: moa_core::events::EventType::ToolCall,
            event: Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input,
                hand_id: None,
            },
            timestamp: now,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn tool_error_event(
        session_id: SessionId,
        tool_id: moa_core::types::identifiers::ToolCallId,
        now: DateTime<Utc>,
    ) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num: 1,
            event_type: moa_core::events::EventType::ToolError,
            event: Event::ToolError {
                tool_id,
                provider_tool_use_id: None,
                tool_name: "bash".to_string(),
                error: "boom".to_string(),
                retryable: false,
            },
            timestamp: now,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[test]
    fn used_skill_effect_is_downgraded_when_its_own_tool_call_failed() {
        // Pins: a used skill whose engaging tool call ends in a durable ToolError is
        // downgraded one step (Resolved would credit Helpful; the failed engagement
        // makes it Mixed), giving skill effects signal independent of the outcome.
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        let experience = resolved_experience_using("data-export", now);
        let tool_id = moa_core::types::identifiers::ToolCallId::new();
        let events = vec![
            tool_call_event(
                experience.session_id,
                tool_id,
                serde_json::json!({"cmd": "python .moa/skills/data-export/scripts/run.py"}),
                now,
            ),
            tool_error_event(experience.session_id, tool_id, now),
        ];

        let attributions = attributions_for_experience(&experience, &events, now);
        let skill_row = attributions
            .iter()
            .find(|row| row.subject_type == AttributionSubjectType::Skill)
            .expect("skill attribution");
        assert_eq!(skill_row.effect, AttributionEffect::Mixed);
        assert_eq!(skill_row.kind, AttributionKind::Standard);
    }

    #[test]
    fn used_skill_effect_is_not_downgraded_by_an_unrelated_tool_error() {
        // Pins: a ToolError on a call that did not engage the skill leaves the skill's
        // outcome-derived effect intact (per-skill matching, not any-error-in-segment).
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        let experience = resolved_experience_using("data-export", now);
        let tool_id = moa_core::types::identifiers::ToolCallId::new();
        let events = vec![
            tool_call_event(
                experience.session_id,
                tool_id,
                serde_json::json!({"cmd": "cargo test"}),
                now,
            ),
            tool_error_event(experience.session_id, tool_id, now),
        ];

        let attributions = attributions_for_experience(&experience, &events, now);
        let skill_row = attributions
            .iter()
            .find(|row| row.subject_type == AttributionSubjectType::Skill)
            .expect("skill attribution");
        assert_eq!(skill_row.effect, AttributionEffect::Helpful);
    }
}
