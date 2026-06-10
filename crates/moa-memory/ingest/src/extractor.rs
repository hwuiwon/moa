//! Fact extractor seam for graph-memory ingestion.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{ExtractedFact, Result, TurnChunk, extract_facts};

/// Extracts graph-memory fact candidates from transcript chunks.
#[async_trait]
pub trait FactExtractor: Send + Sync {
    /// Extracts fact candidates from already chunked turn text.
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>>;
}

/// Deterministic extractor backed by the legacy heuristic implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicFactExtractor;

#[async_trait]
impl FactExtractor for HeuristicFactExtractor {
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        Ok(extract_facts(chunks))
    }
}

/// Deterministic extractor that returns a configured fact script.
#[derive(Debug, Clone)]
pub struct ScriptedFactExtractor {
    facts: Arc<[ExtractedFact]>,
}

impl ScriptedFactExtractor {
    /// Creates a scripted extractor from exact fact DTOs.
    #[must_use]
    pub fn new(facts: Vec<ExtractedFact>) -> Self {
        Self {
            facts: Arc::from(facts.into_boxed_slice()),
        }
    }

    /// Creates a scripted extractor by parsing summary strings into fact DTOs.
    #[must_use]
    pub fn from_summaries<I, S>(summaries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut facts = Vec::new();
        for (index, summary) in summaries.into_iter().enumerate() {
            let text = format!("Fact: {}", summary.into());
            facts.extend(extract_facts(&[TurnChunk {
                index,
                text,
                token_estimate: 1,
            }]));
        }
        Self::new(facts)
    }
}

#[async_trait]
impl FactExtractor for ScriptedFactExtractor {
    async fn extract(&self, _chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        Ok(self.facts.to_vec())
    }
}
