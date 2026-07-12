//! Evidence-backed task-segment assessment types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Assessment artifact produced for one task segment boundary pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentAssessment {
    /// Assigned task outcome.
    pub outcome: SegmentOutcome,
    /// Confidence in the assigned outcome.
    pub confidence: f64,
    /// Assessment phase that produced this result.
    pub phase: AssessmentPhase,
    /// Structured evidence considered by the assessor.
    pub evidence: Vec<SegmentEvidence>,
    /// Timestamp for this assessment pass.
    pub assessed_at: DateTime<Utc>,
    /// Stable assessor policy version.
    pub policy_version: String,
}

/// Task segment outcome labels.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum SegmentOutcome {
    /// The task appears to have been completed successfully.
    Resolved,
    /// The task appears partially completed.
    Partial,
    /// The signals are inconclusive.
    Unknown,
    /// The task appears to have failed.
    Failed,
    /// The task was abandoned or cancelled.
    Abandoned,
}

impl SegmentOutcome {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Segment assessment phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentPhase {
    /// Assessed when the segment was completed.
    Immediate,
    /// Reassessed after a later user message supplied continuation evidence.
    Deferred,
    /// Assessed when no more continuation signals are expected.
    Final,
}

/// Evidence item used by segment assessment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentEvidence {
    /// Evidence source class.
    pub kind: SegmentEvidenceKind,
    /// Direction the evidence points.
    pub polarity: SegmentEvidencePolarity,
    /// Signal strength in `[0.0, 1.0]`.
    pub strength: f64,
    /// Concise human-readable explanation.
    pub summary: String,
}

/// Segment assessment evidence classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentEvidenceKind {
    /// Tool call outcome evidence.
    ToolOutcome,
    /// Verification command evidence.
    Verification,
    /// Later user continuation evidence.
    Continuation,
    /// Agent self-assessment text evidence.
    SelfAssessment,
    /// Structural anomaly evidence.
    Structural,
    /// Explicit lifecycle override evidence.
    Override,
}

/// Direction of a segment evidence item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentEvidencePolarity {
    /// Evidence supports the resolved outcome.
    SupportsResolved,
    /// Evidence supports a partial outcome.
    SupportsPartial,
    /// Evidence supports a failed outcome.
    SupportsFailed,
    /// Evidence supports an abandoned outcome.
    SupportsAbandoned,
    /// Evidence is neutral or inconclusive.
    Neutral,
}

/// Historical structural baseline for one tenant and intent label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentBaseline {
    /// Number of historical segments contributing to the baseline.
    pub sample_count: usize,
    /// Mean turn count.
    pub avg_turns: f64,
    /// Standard deviation of turn count.
    pub stddev_turns: Option<f64>,
    /// Mean token cost.
    pub avg_cost: f64,
    /// Standard deviation of token cost.
    pub stddev_cost: Option<f64>,
    /// Mean segment duration in seconds.
    pub avg_duration_secs: f64,
    /// Standard deviation of segment duration in seconds.
    pub stddev_duration_secs: Option<f64>,
}

/// Resolution-rate aggregate for one skill within a tenant and optional intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillResolutionRate {
    /// Skill name.
    pub skill_name: String,
    /// Number of resolved segments that activated the skill.
    pub uses: u64,
    /// Resolution-rate value in `[0.0, 1.0]`.
    pub resolution_rate: f64,
}
