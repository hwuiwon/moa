//! Ranking comparison and trace formatting for golden retrieval tests.

use std::fmt::Write as _;

use moa_retrieval::retrieval::RetrievalHit;

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
