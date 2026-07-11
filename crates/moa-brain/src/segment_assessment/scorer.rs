//! Composite task-segment assessor.

use chrono::{DateTime, Utc};
use moa_core::{
    config::ResolutionWeights, types::segment_assessment::AssessmentPhase,
    types::segment_assessment::SegmentAssessment, types::segment_assessment::SegmentEvidence,
    types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity, types::segment_assessment::SegmentOutcome,
};

const POLICY_VERSION: &str = "segment-assessment-v1";

/// Special-case rules that override or constrain the assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssessmentOverride {
    /// User or runtime cancelled the task.
    Cancelled,
    /// Agent hit the model-loop turn cap.
    TurnCapExceeded,
    /// A verification command passed.
    VerificationPassed,
    /// A verification command failed.
    VerificationFailed,
    /// Every completed tool call failed.
    AllToolsFailed,
}

/// Composite assessor for task segments.
#[derive(Debug, Clone, Copy, Default)]
pub struct SegmentAssessor {
    weights: ResolutionWeights,
}

impl SegmentAssessor {
    /// Creates an assessor with explicit signal weights.
    #[must_use]
    pub fn new(weights: ResolutionWeights) -> Self {
        Self { weights }
    }

    /// Assesses a segment from available signal values and overrides.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn assess(
        &self,
        tool: Option<f64>,
        verification: Option<f64>,
        continuation: Option<f64>,
        self_assessment: Option<f64>,
        structural: Option<f64>,
        assessed_at: DateTime<Utc>,
        phase: AssessmentPhase,
        overrides: &[AssessmentOverride],
    ) -> SegmentAssessment {
        if overrides.contains(&AssessmentOverride::Cancelled) {
            return segment_assessment(
                SegmentOutcome::Abandoned,
                1.0,
                tool,
                verification,
                continuation,
                self_assessment,
                structural,
                assessed_at,
                phase,
                overrides,
            );
        }
        if overrides.contains(&AssessmentOverride::TurnCapExceeded) {
            return segment_assessment(
                SegmentOutcome::Failed,
                0.9,
                tool,
                verification,
                continuation,
                self_assessment,
                structural,
                assessed_at,
                phase,
                overrides,
            );
        }
        if overrides.contains(&AssessmentOverride::AllToolsFailed) {
            return segment_assessment(
                SegmentOutcome::Failed,
                0.9,
                tool,
                verification,
                continuation,
                self_assessment,
                structural,
                assessed_at,
                phase,
                overrides,
            );
        }

        let mut success_score = weighted_average(
            &[
                (tool, self.weights.tool),
                (verification, self.weights.verification),
                (continuation, self.weights.continuation),
                (self_assessment, self.weights.self_assessment),
                (structural, self.weights.structural),
            ],
            0.4,
        );

        if overrides.contains(&AssessmentOverride::VerificationPassed) {
            success_score = success_score.max(0.50);
        }
        if overrides.contains(&AssessmentOverride::VerificationFailed) {
            success_score = success_score.min(0.49);
        }

        let outcome = outcome_for_score(success_score);
        let confidence = outcome_confidence(success_score, outcome);
        segment_assessment(
            outcome,
            confidence,
            tool,
            verification,
            continuation,
            self_assessment,
            structural,
            assessed_at,
            phase,
            overrides,
        )
    }
}

fn weighted_average(signals: &[(Option<f64>, f64)], default_score: f64) -> f64 {
    let mut weighted_sum = 0.0;
    let mut weight_sum = 0.0;
    for (signal, weight) in signals {
        if let Some(signal) = signal {
            weighted_sum += signal.clamp(0.0, 1.0) * weight;
            weight_sum += weight;
        }
    }
    if weight_sum == 0.0 {
        default_score
    } else {
        (weighted_sum / weight_sum).clamp(0.0, 1.0)
    }
}

fn outcome_for_score(score: f64) -> SegmentOutcome {
    if score >= 0.70 {
        SegmentOutcome::Resolved
    } else if score >= 0.50 {
        SegmentOutcome::Partial
    } else if score >= 0.30 {
        SegmentOutcome::Unknown
    } else if score >= 0.10 {
        SegmentOutcome::Failed
    } else {
        SegmentOutcome::Abandoned
    }
}

fn outcome_confidence(score: f64, outcome: SegmentOutcome) -> f64 {
    match outcome {
        SegmentOutcome::Resolved | SegmentOutcome::Partial => score,
        SegmentOutcome::Unknown => {
            let distance_from_center = (score - 0.40).abs();
            (0.60 - distance_from_center).clamp(0.5, 0.6)
        }
        SegmentOutcome::Failed | SegmentOutcome::Abandoned => 1.0 - score,
        _ => 0.5,
    }
    .clamp(0.0, 1.0)
}

#[allow(clippy::too_many_arguments)]
fn segment_assessment(
    outcome: SegmentOutcome,
    confidence: f64,
    tool: Option<f64>,
    verification: Option<f64>,
    continuation: Option<f64>,
    self_assessment: Option<f64>,
    structural: Option<f64>,
    assessed_at: DateTime<Utc>,
    phase: AssessmentPhase,
    overrides: &[AssessmentOverride],
) -> SegmentAssessment {
    SegmentAssessment {
        outcome,
        confidence: confidence.clamp(0.0, 1.0),
        phase,
        evidence: evidence_items(
            tool,
            verification,
            continuation,
            self_assessment,
            structural,
            overrides,
        ),
        assessed_at,
        policy_version: POLICY_VERSION.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn evidence_items(
    tool: Option<f64>,
    verification: Option<f64>,
    continuation: Option<f64>,
    self_assessment: Option<f64>,
    structural: Option<f64>,
    overrides: &[AssessmentOverride],
) -> Vec<SegmentEvidence> {
    let mut evidence = Vec::new();
    for (kind, value, summary) in [
        (
            SegmentEvidenceKind::ToolOutcome,
            tool,
            "tool outcome signal",
        ),
        (
            SegmentEvidenceKind::Verification,
            verification,
            "verification command signal",
        ),
        (
            SegmentEvidenceKind::Continuation,
            continuation,
            "user continuation signal",
        ),
        (
            SegmentEvidenceKind::SelfAssessment,
            self_assessment,
            "agent self-assessment signal",
        ),
        (
            SegmentEvidenceKind::Structural,
            structural,
            "segment structural signal",
        ),
    ] {
        if let Some(strength) = value {
            evidence.push(SegmentEvidence {
                kind,
                polarity: polarity_for_strength(strength),
                strength: strength.clamp(0.0, 1.0),
                summary: summary.to_string(),
            });
        }
    }
    evidence.extend(overrides.iter().map(override_evidence));
    evidence
}

fn override_evidence(override_value: &AssessmentOverride) -> SegmentEvidence {
    match override_value {
        AssessmentOverride::Cancelled => SegmentEvidence {
            kind: SegmentEvidenceKind::Override,
            polarity: SegmentEvidencePolarity::SupportsAbandoned,
            strength: 1.0,
            summary: "session cancellation closed the segment".to_string(),
        },
        AssessmentOverride::TurnCapExceeded => SegmentEvidence {
            kind: SegmentEvidenceKind::Override,
            polarity: SegmentEvidencePolarity::SupportsFailed,
            strength: 0.9,
            summary: "model-loop turn cap was reached".to_string(),
        },
        AssessmentOverride::VerificationPassed => SegmentEvidence {
            kind: SegmentEvidenceKind::Override,
            polarity: SegmentEvidencePolarity::SupportsResolved,
            strength: 0.9,
            summary: "verification passed".to_string(),
        },
        AssessmentOverride::VerificationFailed => SegmentEvidence {
            kind: SegmentEvidenceKind::Override,
            polarity: SegmentEvidencePolarity::SupportsFailed,
            strength: 0.9,
            summary: "verification failed".to_string(),
        },
        AssessmentOverride::AllToolsFailed => SegmentEvidence {
            kind: SegmentEvidenceKind::Override,
            polarity: SegmentEvidencePolarity::SupportsFailed,
            strength: 0.9,
            summary: "all completed tool calls failed".to_string(),
        },
    }
}

fn polarity_for_strength(strength: f64) -> SegmentEvidencePolarity {
    if strength >= 0.70 {
        SegmentEvidencePolarity::SupportsResolved
    } else if strength >= 0.50 {
        SegmentEvidencePolarity::SupportsPartial
    } else if strength >= 0.30 {
        SegmentEvidencePolarity::Neutral
    } else if strength >= 0.10 {
        SegmentEvidencePolarity::SupportsFailed
    } else {
        SegmentEvidencePolarity::SupportsAbandoned
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{
        config::ResolutionWeights, types::segment_assessment::AssessmentPhase,
        types::segment_assessment::SegmentEvidenceKind,
        types::segment_assessment::SegmentEvidencePolarity,
        types::segment_assessment::SegmentOutcome,
    };

    use super::{AssessmentOverride, SegmentAssessor};

    fn assessed_at() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 11, 0, 0, 0)
            .single()
            .expect("fixed assessment timestamp must be valid")
    }

    #[test]
    fn null_signals_are_excluded_and_weights_renormalized() {
        let assessor = SegmentAssessor::new(ResolutionWeights::default());
        let assessment = assessor.assess(
            Some(0.8),
            None,
            None,
            Some(0.7),
            None,
            assessed_at(),
            AssessmentPhase::Immediate,
            &[],
        );

        assert_eq!(assessment.outcome, SegmentOutcome::Resolved);
        assert!(assessment.confidence >= 0.7);
        assert_eq!(assessment.evidence.len(), 2);
    }

    #[test]
    fn cancellation_overrides_to_abandoned() {
        let assessor = SegmentAssessor::default();
        let timestamp = assessed_at();
        let assessment = assessor.assess(
            Some(0.8),
            Some(0.95),
            Some(0.85),
            Some(0.7),
            Some(0.6),
            timestamp,
            AssessmentPhase::Final,
            &[AssessmentOverride::Cancelled],
        );

        assert_eq!(assessment.outcome, SegmentOutcome::Abandoned);
        assert_eq!(assessment.confidence, 1.0);
        assert_eq!(assessment.assessed_at, timestamp);
        assert!(assessment.evidence.iter().any(|evidence| {
            evidence.kind == SegmentEvidenceKind::Override
                && evidence.polarity == SegmentEvidencePolarity::SupportsAbandoned
        }));
    }

    #[test]
    fn turn_cap_overrides_to_failed() {
        let assessor = SegmentAssessor::default();
        let assessment = assessor.assess(
            Some(0.8),
            Some(0.95),
            None,
            Some(0.7),
            None,
            assessed_at(),
            AssessmentPhase::Immediate,
            &[AssessmentOverride::TurnCapExceeded],
        );

        assert_eq!(assessment.outcome, SegmentOutcome::Failed);
        assert!(assessment.confidence >= 0.7);
    }

    #[test]
    fn verification_pass_floors_assessment_to_partial() {
        let assessor = SegmentAssessor::default();
        let assessment = assessor.assess(
            Some(0.1),
            Some(0.95),
            None,
            Some(0.15),
            None,
            assessed_at(),
            AssessmentPhase::Immediate,
            &[AssessmentOverride::VerificationPassed],
        );

        assert!(matches!(
            assessment.outcome,
            SegmentOutcome::Partial | SegmentOutcome::Resolved
        ));
    }

    #[test]
    fn all_tools_failed_overrides_to_failed_with_high_confidence() {
        let assessor = SegmentAssessor::default();
        let assessment = assessor.assess(
            Some(0.1),
            None,
            None,
            Some(0.5),
            None,
            assessed_at(),
            AssessmentPhase::Immediate,
            &[AssessmentOverride::AllToolsFailed],
        );

        assert_eq!(assessment.outcome, SegmentOutcome::Failed);
        assert!(assessment.confidence >= 0.7);
    }

    #[test]
    fn absent_signals_do_not_create_placeholder_evidence() {
        let assessor = SegmentAssessor::default();
        let assess = || {
            assessor.assess(
                None,
                None,
                None,
                None,
                None,
                assessed_at(),
                AssessmentPhase::Immediate,
                &[],
            )
        };
        let assessment = assess();

        assert_eq!(assessment.evidence, Vec::new());
        assert_eq!(assessment.assessed_at, assessed_at());
        assert_eq!(assessment, assess());
    }
}
