//! Learning-log helper types for turn workflows.

use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError, types::identifiers::SegmentId, types::identifiers::TenantId,
    types::learning::LearningEntry, types::segment_assessment::SegmentAssessment,
};
use moa_core::{events::Event, types::events_stream::EventRecord};
use uuid::Uuid;

/// Request for building a learning-log entry from a segment assessment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentAssessmentLearningRequest<'a> {
    /// Stable entry identifier.
    pub(crate) id: Uuid,
    /// Tenant scope for the learning entry.
    pub(crate) tenant_id: TenantId,
    /// Segment assessed by the turn driver.
    pub(crate) segment_id: SegmentId,
    /// Assessment payload to persist.
    pub(crate) assessment: &'a SegmentAssessment,
    /// Valid-from timestamp for this learning version.
    pub(crate) valid_from: DateTime<Utc>,
}

/// Builds the learning-log entry for a segment assessment.
pub(crate) fn segment_assessment_learning_entry(
    request: SegmentAssessmentLearningRequest<'_>,
) -> Result<LearningEntry, MoaError> {
    Ok(LearningEntry {
        id: request.id,
        tenant_id: request.tenant_id,
        learning_type: "segment_assessed".to_string(),
        target_id: request.segment_id.to_string(),
        target_label: Some(request.assessment.outcome.as_str().to_string()),
        payload: serde_json::to_value(request.assessment).map_err(|error| {
            MoaError::StorageError(format!(
                "serialize segment assessment learning payload: {error}"
            ))
        })?,
        confidence: Some(request.assessment.confidence),
        source_refs: vec![request.segment_id.0],
        actor: "system".to_string(),
        valid_from: request.valid_from,
        valid_to: None,
        batch_id: None,
        version: 1,
    })
}

/// Returns whether an assessed segment should dispatch skill-learning follow-up.
///
/// Mirrors the distiller's own gates (learnable outcome plus tool-call
/// threshold) so unlearnable experiences never spawn a detached workflow that
/// would load session data only to skip.
pub(crate) fn skill_learning_dispatch_is_eligible(
    segment_events: &[EventRecord],
    min_tool_calls: usize,
    experience: &moa_core::types::experience::ExperienceRecord,
    attributions: &[moa_core::types::experience::ExperienceAttribution],
) -> bool {
    moa_skills::distiller::experience_is_learnable(experience, attributions)
        && segment_events
            .iter()
            .filter(|record| matches!(record.event, Event::ToolCall { .. }))
            .count()
            >= min_tool_calls
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        types::identifiers::SegmentId, types::identifiers::TenantId,
        types::segment_assessment::AssessmentPhase, types::segment_assessment::SegmentAssessment,
        types::segment_assessment::SegmentEvidence, types::segment_assessment::SegmentOutcome,
    };

    use super::{SegmentAssessmentLearningRequest, segment_assessment_learning_entry};

    #[test]
    fn segment_assessment_learning_entry_preserves_assessment_identity() {
        // Pins: segment assessment learning records keep stable target and confidence fields.
        let now = Utc::now();
        let segment_id = SegmentId(uuid::Uuid::now_v7());
        let assessment = SegmentAssessment {
            outcome: SegmentOutcome::Resolved,
            confidence: 0.9,
            phase: AssessmentPhase::Final,
            evidence: Vec::<SegmentEvidence>::new(),
            assessed_at: now,
            policy_version: "test".to_string(),
        };

        let entry = segment_assessment_learning_entry(SegmentAssessmentLearningRequest {
            id: uuid::Uuid::now_v7(),
            tenant_id: TenantId::new(),
            segment_id,
            assessment: &assessment,
            valid_from: now,
        })
        .expect("assessment should serialize");

        assert_eq!(entry.learning_type, "segment_assessed");
        assert_eq!(entry.target_id, segment_id.to_string());
        assert_eq!(entry.target_label, Some("resolved".to_string()));
        assert_eq!(entry.confidence, Some(0.9));
        assert_eq!(entry.source_refs, vec![segment_id.0]);
    }
}
