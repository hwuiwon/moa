//! Memory-recall metrics for long-conversation eval runs.

use moa_core::{Event, EventFilter, EventType, SessionId, SessionStore};

/// Scenario expectations needed for memory-recall scoring.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryScenario {
    /// Facts planted in the conversation that should be recalled later.
    pub planted_facts: Vec<String>,
    /// Retrieval cutoff used for recall@K.
    pub recall_k: usize,
}

/// Counts consolidation outcomes observed in the session event stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsolidationOutcomes {
    /// Successful consolidation-like memory ingest events.
    pub successes: usize,
    /// Failed consolidation-like events.
    pub failures: usize,
    /// Skipped consolidation-like warning events.
    pub skipped: usize,
}

/// Computes planted-fact recall by searching the session store for each fact.
pub async fn compute_planted_fact_recall(
    scenario: &MemoryScenario,
    session_id: SessionId,
    session_store: &dyn SessionStore,
) -> moa_core::Result<f64> {
    if scenario.planted_facts.is_empty() {
        return Ok(0.0);
    }

    let mut recalled = 0_usize;
    let limit = scenario.recall_k.max(1);
    for fact in &scenario.planted_facts {
        let hits = session_store
            .search_events(
                fact,
                EventFilter {
                    session_id: Some(session_id),
                    limit: Some(limit),
                    ..EventFilter::default()
                },
            )
            .await?;
        if !hits.is_empty() {
            recalled += 1;
        }
    }

    Ok(recalled as f64 / scenario.planted_facts.len() as f64)
}

/// Counts memory write events in a session event list.
#[must_use]
pub fn count_pages_written(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::MemoryWrite { .. }))
        .count()
}

/// Counts memory consolidation-like outcomes from session events.
#[must_use]
pub fn count_consolidation_outcomes(events: &[Event]) -> ConsolidationOutcomes {
    let mut outcomes = ConsolidationOutcomes::default();
    for event in events {
        match event {
            Event::MemoryIngest { .. } => outcomes.successes += 1,
            Event::Error { message, .. }
                if message.to_ascii_lowercase().contains("consolidation") =>
            {
                outcomes.failures += 1;
            }
            Event::Warning { message }
                if message.to_ascii_lowercase().contains("consolidation") =>
            {
                outcomes.skipped += 1;
            }
            _ => {}
        }
    }
    outcomes
}

/// Returns an event-type filter for memory-recall queries.
#[must_use]
pub fn memory_event_types() -> Vec<EventType> {
    vec![
        EventType::MemoryRead,
        EventType::MemoryWrite,
        EventType::MemoryIngest,
        EventType::BrainResponse,
    ]
}
