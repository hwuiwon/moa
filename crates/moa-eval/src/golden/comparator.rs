//! Ranking comparison and trace formatting for golden retrieval tests.

use std::fmt::{self, Write as _};

use moa_brain::retrieval::RetrievalHit;
use uuid::Uuid;

/// Details for one expected hit that was not found in its accepted rank window.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedRankMismatch {
    /// Expected node uid.
    pub expected_uid: Uuid,
    /// Zero-based expected rank.
    pub expected_rank: usize,
    /// Inclusive lower bound of the accepted rank window.
    pub window_start: usize,
    /// Exclusive upper bound of the accepted rank window.
    pub window_end: usize,
    /// Score at the expected rank, when an actual hit exists there.
    pub expected_rank_score: Option<f64>,
}

/// Error returned when actual retrieval order diverges from the golden ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct GoldenRankingMismatch {
    /// Ranking mismatches in expected-order sequence.
    pub mismatches: Vec<ExpectedRankMismatch>,
    /// Human-readable trace of the actual hits.
    pub trace: String,
}

impl fmt::Display for GoldenRankingMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "golden retrieval ranking mismatch ({} mismatch(es))",
            self.mismatches.len()
        )?;
        for mismatch in &self.mismatches {
            writeln!(
                formatter,
                "expected {} near rank {} (accepted ranks {}..{}, rank score {:?})",
                mismatch.expected_uid,
                mismatch.expected_rank,
                mismatch.window_start,
                mismatch.window_end,
                mismatch.expected_rank_score
            )?;
        }
        write!(formatter, "{}", self.trace)
    }
}

impl std::error::Error for GoldenRankingMismatch {}

/// Compares actual hits against an expected top-K list with a rank window and score epsilon.
pub fn compare_top_k_within_window(
    hits: &[RetrievalHit],
    expected: &[Uuid],
    window: usize,
    score_eps: f64,
) -> Result<(), GoldenRankingMismatch> {
    let mut mismatches = Vec::new();
    for (rank, expected_uid) in expected.iter().enumerate() {
        let window_start = rank.saturating_sub(window);
        let window_end = (rank + window + 1).min(hits.len());
        let found_in_window = hits
            .get(window_start..window_end)
            .unwrap_or_default()
            .iter()
            .any(|hit| hit.uid == *expected_uid);
        let expected_rank_score = hits.get(rank).map(|hit| hit.score);
        let found_by_score_tie = expected_rank_score.is_some_and(|rank_score| {
            hits.iter()
                .any(|hit| hit.uid == *expected_uid && (rank_score - hit.score).abs() < score_eps)
        });

        if !found_in_window && !found_by_score_tie {
            mismatches.push(ExpectedRankMismatch {
                expected_uid: *expected_uid,
                expected_rank: rank,
                window_start,
                window_end,
                expected_rank_score,
            });
        }
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(GoldenRankingMismatch {
            mismatches,
            trace: dump_traces(hits),
        })
    }
}

/// Panics if actual hits do not satisfy the golden top-K comparator.
pub fn assert_top_k_within_window(
    hits: &[RetrievalHit],
    expected: &[Uuid],
    window: usize,
    score_eps: f64,
) {
    if let Err(error) = compare_top_k_within_window(hits, expected, window, score_eps) {
        panic!("{error}");
    }
}

/// Formats a retrieval trace with uid, score, contributing legs, and node name.
#[must_use]
pub fn dump_traces(hits: &[RetrievalHit]) -> String {
    let mut trace = String::new();
    for (rank, hit) in hits.iter().enumerate() {
        let _ = writeln!(
            trace,
            "rank={} uid={} score={:.4} legs={:?} name={}",
            rank, hit.uid, hit.score, hit.legs, hit.node.name
        );
    }
    trace
}
