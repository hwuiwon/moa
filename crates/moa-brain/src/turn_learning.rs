//! Construction helpers for segment-derived learning artifacts.

use chrono::{DateTime, Utc};
use moa_core::{
    types::experience::ExperienceAttribution, types::experience::ExperienceRecord,
    types::experience::LearningCandidate, types::segment_assessment::SegmentAssessment,
    types::segments::TaskSegment, types::session::SessionMeta,
};
use moa_skills::evidence::SanitizedLearningEvidence;

use crate::query_rewrite::QueryRewriteResult;

use crate::learning::{
    attribution::attributions_for_experience, candidates::propose_candidates_for_experience,
    experience::experience_from_assessment,
};

/// Learning artifacts derived from one assessed segment.
#[derive(Clone, Debug)]
pub struct SegmentLearningBundle {
    /// Experience row produced for the assessed segment.
    pub experience: ExperienceRecord,
    /// Attribution rows derived from the experience and its source events.
    pub attributions: Vec<ExperienceAttribution>,
    /// Reviewable learning candidates proposed from the experience.
    pub candidates: Vec<LearningCandidate>,
}

/// Builds all learning artifacts for an assessed segment without persisting them.
///
/// The bundle is derived entirely from sanitized segment evidence, so every row
/// it produces — experience, attribution, and candidate — carries redacted
/// content. The raw session event log remains the separate source-of-truth owner
/// of the unredacted transcript.
#[must_use]
pub fn build_segment_learning_bundle(
    meta: &SessionMeta,
    segment: &TaskSegment,
    assessment: &SegmentAssessment,
    evidence: &SanitizedLearningEvidence,
    rewrite: Option<&QueryRewriteResult>,
    duration_ms: Option<u64>,
    now: DateTime<Utc>,
) -> SegmentLearningBundle {
    let experience = experience_from_assessment(
        meta,
        segment,
        assessment,
        evidence,
        rewrite,
        duration_ms,
        now,
    );
    let attributions = attributions_for_experience(&experience, evidence, now);
    let candidates = propose_candidates_for_experience(&experience, &attributions, now);
    SegmentLearningBundle {
        experience,
        attributions,
        candidates,
    }
}
