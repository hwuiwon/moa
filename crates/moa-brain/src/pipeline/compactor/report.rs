//! Compaction reporting types used for processor output metadata.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CompactionTier {
    Tier1Deterministic,
    Tier2CacheAware,
    Tier3Summarization,
}

#[derive(Debug, Clone, Default)]
pub(super) struct CompactionReport {
    pub(super) tiers_applied: Vec<CompactionTier>,
    pub(super) tokens_before: usize,
    pub(super) tokens_after: usize,
    pub(super) messages_elided: usize,
    pub(super) summary_text: Option<String>,
    pub(super) events_summarized: Option<usize>,
}

impl CompactionReport {
    pub(super) fn tokens_reclaimed(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}
