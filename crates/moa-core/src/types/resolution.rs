//! Automated task-resolution scoring types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Composite score and signal breakdown for one task segment resolution pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionScore {
    /// Assigned resolution label.
    pub label: ResolutionLabel,
    /// Confidence in the assigned label.
    pub confidence: f64,
    /// Tool-outcome signal score.
    pub tool_signal: Option<f64>,
    /// Verification-command signal score.
    pub verification_signal: Option<f64>,
    /// User-continuation signal score.
    pub continuation_signal: Option<f64>,
    /// Agent self-assessment signal score.
    pub self_assessment_signal: Option<f64>,
    /// Structural-anomaly signal score.
    pub structural_signal: Option<f64>,
    /// Timestamp for this scoring pass.
    pub scored_at: DateTime<Utc>,
    /// Scoring phase that produced this result.
    pub scoring_phase: ScoringPhase,
}

/// Task segment resolution labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResolutionLabel {
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

impl ResolutionLabel {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

impl std::fmt::Display for ResolutionLabel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Resolution scoring phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoringPhase {
    /// Scored when the segment was completed.
    Immediate,
    /// Re-scored after a later user message supplied continuation evidence.
    Deferred,
    /// Scored when no more continuation signals are expected.
    Final,
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
    /// Resolution-rate score in `[0.0, 1.0]`.
    pub resolution_rate: f64,
    /// Average token cost for matching segments.
    pub avg_token_cost: f64,
    /// Average turn count for matching segments.
    pub avg_turn_count: f64,
}
