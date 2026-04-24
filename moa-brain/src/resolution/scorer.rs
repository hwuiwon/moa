//! Composite task-resolution scorer.

use chrono::Utc;
use moa_core::{ResolutionLabel, ResolutionScore, ResolutionWeights, ScoringPhase};

/// Special-case rules that override or constrain the composite score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionOverride {
    /// User or runtime cancelled the task.
    Cancelled,
    /// Agent hit the turn budget.
    TurnBudgetExceeded,
    /// A verification command passed.
    VerificationPassed,
    /// A verification command failed.
    VerificationFailed,
    /// Every completed tool call failed.
    AllToolsFailed,
}

/// Composite scorer for automated task resolution.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolutionScorer {
    weights: ResolutionWeights,
}

impl ResolutionScorer {
    /// Creates a scorer with explicit signal weights.
    #[must_use]
    pub fn new(weights: ResolutionWeights) -> Self {
        Self { weights }
    }

    /// Scores a segment from available signal values and overrides.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn score(
        &self,
        tool: Option<f64>,
        verification: Option<f64>,
        continuation: Option<f64>,
        self_assessment: Option<f64>,
        structural: Option<f64>,
        phase: ScoringPhase,
        overrides: &[ResolutionOverride],
    ) -> ResolutionScore {
        if overrides.contains(&ResolutionOverride::Cancelled) {
            return resolution_score(
                ResolutionLabel::Abandoned,
                1.0,
                tool,
                verification,
                continuation,
                self_assessment,
                structural,
                phase,
            );
        }
        if overrides.contains(&ResolutionOverride::TurnBudgetExceeded) {
            return resolution_score(
                ResolutionLabel::Failed,
                0.9,
                tool,
                verification,
                continuation,
                self_assessment,
                structural,
                phase,
            );
        }
        if overrides.contains(&ResolutionOverride::AllToolsFailed) {
            return resolution_score(
                ResolutionLabel::Failed,
                0.9,
                tool,
                verification,
                continuation,
                self_assessment,
                structural,
                phase,
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

        if overrides.contains(&ResolutionOverride::VerificationPassed) {
            success_score = success_score.max(0.50);
        }
        if overrides.contains(&ResolutionOverride::VerificationFailed) {
            success_score = success_score.min(0.49);
        }

        let label = label_for_score(success_score);
        let confidence = label_confidence(success_score, label);
        resolution_score(
            label,
            confidence,
            tool,
            verification,
            continuation,
            self_assessment,
            structural,
            phase,
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

fn label_for_score(score: f64) -> ResolutionLabel {
    if score >= 0.70 {
        ResolutionLabel::Resolved
    } else if score >= 0.50 {
        ResolutionLabel::Partial
    } else if score >= 0.30 {
        ResolutionLabel::Unknown
    } else if score >= 0.10 {
        ResolutionLabel::Failed
    } else {
        ResolutionLabel::Abandoned
    }
}

fn label_confidence(score: f64, label: ResolutionLabel) -> f64 {
    match label {
        ResolutionLabel::Resolved | ResolutionLabel::Partial => score,
        ResolutionLabel::Unknown => {
            let distance_from_center = (score - 0.40).abs();
            (0.60 - distance_from_center).clamp(0.5, 0.6)
        }
        ResolutionLabel::Failed | ResolutionLabel::Abandoned => 1.0 - score,
        _ => 0.5,
    }
    .clamp(0.0, 1.0)
}

#[allow(clippy::too_many_arguments)]
fn resolution_score(
    label: ResolutionLabel,
    confidence: f64,
    tool_signal: Option<f64>,
    verification_signal: Option<f64>,
    continuation_signal: Option<f64>,
    self_assessment_signal: Option<f64>,
    structural_signal: Option<f64>,
    scoring_phase: ScoringPhase,
) -> ResolutionScore {
    ResolutionScore {
        label,
        confidence: confidence.clamp(0.0, 1.0),
        tool_signal,
        verification_signal,
        continuation_signal,
        self_assessment_signal,
        structural_signal,
        scored_at: Utc::now(),
        scoring_phase,
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{ResolutionLabel, ResolutionWeights, ScoringPhase};

    use super::{ResolutionOverride, ResolutionScorer};

    #[test]
    fn null_signals_are_excluded_and_weights_renormalized() {
        let scorer = ResolutionScorer::new(ResolutionWeights::default());
        let score = scorer.score(
            Some(0.8),
            None,
            None,
            Some(0.7),
            None,
            ScoringPhase::Immediate,
            &[],
        );

        assert_eq!(score.label, ResolutionLabel::Resolved);
        assert!(score.confidence >= 0.7);
    }

    #[test]
    fn cancellation_overrides_to_abandoned() {
        let scorer = ResolutionScorer::default();
        let score = scorer.score(
            Some(0.8),
            Some(0.95),
            Some(0.85),
            Some(0.7),
            Some(0.6),
            ScoringPhase::Final,
            &[ResolutionOverride::Cancelled],
        );

        assert_eq!(score.label, ResolutionLabel::Abandoned);
        assert_eq!(score.confidence, 1.0);
    }

    #[test]
    fn turn_budget_overrides_to_failed() {
        let scorer = ResolutionScorer::default();
        let score = scorer.score(
            Some(0.8),
            Some(0.95),
            None,
            Some(0.7),
            None,
            ScoringPhase::Immediate,
            &[ResolutionOverride::TurnBudgetExceeded],
        );

        assert_eq!(score.label, ResolutionLabel::Failed);
        assert!(score.confidence >= 0.7);
    }

    #[test]
    fn verification_pass_floors_score_to_partial() {
        let scorer = ResolutionScorer::default();
        let score = scorer.score(
            Some(0.1),
            Some(0.95),
            None,
            Some(0.15),
            None,
            ScoringPhase::Immediate,
            &[ResolutionOverride::VerificationPassed],
        );

        assert!(matches!(
            score.label,
            ResolutionLabel::Partial | ResolutionLabel::Resolved
        ));
    }

    #[test]
    fn all_tools_failed_overrides_to_failed_with_high_confidence() {
        let scorer = ResolutionScorer::default();
        let score = scorer.score(
            Some(0.1),
            None,
            None,
            Some(0.5),
            None,
            ScoringPhase::Immediate,
            &[ResolutionOverride::AllToolsFailed],
        );

        assert_eq!(score.label, ResolutionLabel::Failed);
        assert!(score.confidence >= 0.7);
    }
}
