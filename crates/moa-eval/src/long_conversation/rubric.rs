//! Failure-mode rubric checks over a completed long-conversation run.
//!
//! Maps the "over-optimism" failure mode — declaring success without evidence —
//! onto the deterministic long-conversation report. The long-conversation
//! harness has no LLM judge; its verdict is the deterministic score card plus
//! the lineage score records already carried on [`LongRunReport`]. So this
//! rubric is a post-run check over those score records rather than a new judge
//! input: it flags a run whose functional score declares the task complete while
//! the turn's citation-verification signal is negative (at least one
//! `citation_verified` score is `false`).

use moa_lineage_core::{ScoreRecord, ScoreValue};

use super::score_card::ScoreCard;

/// Lineage score name emitted per citation by the grounding cascade
/// (`moa_lineage_citation::emit_verifier_scores`).
const CITATION_VERIFIED_SCORE: &str = "citation_verified";

/// Outcome of the "declared success without evidence" rubric check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredSuccessWithoutEvidence {
    /// Whether the run's functional score declared the task complete.
    pub declared_success: bool,
    /// Count of `citation_verified` scores that failed verification.
    pub unverified_citations: usize,
    /// Count of `citation_verified` scores that passed verification.
    pub verified_citations: usize,
}

impl DeclaredSuccessWithoutEvidence {
    /// Whether the run is flagged as over-optimistic: it declared success while
    /// at least one citation failed verification. A run with no citation
    /// evidence at all is not flagged — absence of evidence is a separate
    /// (recall) failure mode, not over-optimism.
    #[must_use]
    pub fn flagged(&self) -> bool {
        self.declared_success && self.unverified_citations > 0
    }
}

/// Evaluates the over-optimism rubric over a score card and its lineage score
/// records.
///
/// `score_records` are the run's `analytics.scores` rows, exactly as carried on
/// [`super::LongRunReport::score_records`]; the citation-verification signal is
/// read from the `citation_verified` rows the grounding cascade emitted for the
/// run.
#[must_use]
pub fn declared_success_without_evidence(
    score_card: &ScoreCard,
    score_records: &[ScoreRecord],
) -> DeclaredSuccessWithoutEvidence {
    let mut verified_citations = 0;
    let mut unverified_citations = 0;
    for record in score_records {
        if record.name != CITATION_VERIFIED_SCORE {
            continue;
        }
        match record.value {
            ScoreValue::Boolean(true) => verified_citations += 1,
            ScoreValue::Boolean(false) => unverified_citations += 1,
            _ => {}
        }
    }
    DeclaredSuccessWithoutEvidence {
        declared_success: score_card.functional.task_completed,
        unverified_citations,
        verified_citations,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{types::identifiers::StoragePartitionId, types::identifiers::UserId};
    use moa_lineage_core::{ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, TurnId};
    use uuid::Uuid;

    use super::super::score_card::{FunctionalScores, ScoreCard};
    use super::*;

    fn citation_verified_record(verified: bool) -> ScoreRecord {
        ScoreRecord {
            score_id: Uuid::now_v7(),
            ts: Utc::now(),
            target: ScoreTarget::Turn {
                turn_id: TurnId::new_v7(),
            },
            storage_partition_id: StoragePartitionId::new("tenant"),
            user_id: Some(UserId::new("user")),
            name: CITATION_VERIFIED_SCORE.to_string(),
            value: ScoreValue::Boolean(verified),
            source: ScoreSource::OnlineJudge,
            model_or_evaluator: "bm25+lexical_overlap".to_string(),
            run_id: None,
            dataset_id: None,
            comment: None,
        }
    }

    fn score_card(task_completed: bool) -> ScoreCard {
        ScoreCard {
            functional: FunctionalScores {
                task_completed,
                ..FunctionalScores::default()
            },
            ..ScoreCard::default()
        }
    }

    #[test]
    fn declared_success_with_unverified_citation_is_flagged() {
        // Pins: a run that declares the task complete while a citation failed
        // verification is the over-optimism failure mode and must flag.
        let card = score_card(true);
        let records = vec![
            citation_verified_record(true),
            citation_verified_record(false),
        ];

        let outcome = declared_success_without_evidence(&card, &records);

        assert!(outcome.flagged());
        assert!(outcome.declared_success);
        assert_eq!(outcome.unverified_citations, 1);
        assert_eq!(outcome.verified_citations, 1);
    }

    #[test]
    fn declared_success_with_all_citations_verified_is_not_flagged() {
        // Pins: grounded success (every citation verified) is not over-optimism.
        let card = score_card(true);
        let records = vec![
            citation_verified_record(true),
            citation_verified_record(true),
        ];

        let outcome = declared_success_without_evidence(&card, &records);

        assert!(!outcome.flagged());
        assert_eq!(outcome.unverified_citations, 0);
        assert_eq!(outcome.verified_citations, 2);
    }

    #[test]
    fn unverified_citation_without_declared_success_is_not_flagged() {
        // Pins: the flag requires the declared-success side too; a failed
        // citation on a run that did not declare success is not over-optimism.
        let card = score_card(false);
        let records = vec![citation_verified_record(false)];

        let outcome = declared_success_without_evidence(&card, &records);

        assert!(!outcome.flagged());
        assert!(!outcome.declared_success);
        assert_eq!(outcome.unverified_citations, 1);
    }

    #[test]
    fn declared_success_without_any_citation_evidence_is_not_flagged() {
        // Pins: absence of citation evidence is a recall failure, not
        // over-optimism; the rubric only fires on a negative verification.
        let card = score_card(true);

        let outcome = declared_success_without_evidence(&card, &[]);

        assert!(!outcome.flagged());
        assert_eq!(outcome.unverified_citations, 0);
        assert_eq!(outcome.verified_citations, 0);
    }
}
