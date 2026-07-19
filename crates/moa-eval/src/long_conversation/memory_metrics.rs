//! Memory-recall metrics for long-conversation eval runs.

use std::collections::HashSet;

use moa_core::{
    events::Event, traits::SessionStore, types::events_stream::EventFilter,
    types::identifiers::SessionId, types::identifiers::ToolCallId,
};

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
) -> moa_core::error::Result<f64> {
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
    let successful_tool_ids = successful_tool_ids(events);
    events
        .iter()
        .filter(|event| match event {
            Event::MemoryWrite { .. } => true,
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } if successful_tool_ids.contains(tool_id) => is_memory_write_tool(tool_name, input),
            _ => false,
        })
        .count()
}

/// Counts memory consolidation-like outcomes from session events.
#[must_use]
pub fn count_consolidation_outcomes(events: &[Event]) -> ConsolidationOutcomes {
    let mut outcomes = ConsolidationOutcomes::default();
    let memory_tool_calls = memory_tool_calls(events);
    for event in events {
        match event {
            Event::MemoryIngest { .. } => outcomes.successes += 1,
            Event::ToolResult {
                tool_id, success, ..
            } if memory_tool_calls.contains(tool_id) => {
                if *success {
                    outcomes.successes += 1;
                } else {
                    outcomes.failures += 1;
                }
            }
            Event::ToolError { tool_id, .. } if memory_tool_calls.contains(tool_id) => {
                outcomes.failures += 1;
            }
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

fn successful_tool_ids(events: &[Event]) -> HashSet<ToolCallId> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolResult {
                tool_id, success, ..
            } if *success => Some(*tool_id),
            _ => None,
        })
        .collect()
}

fn memory_tool_calls(events: &[Event]) -> HashSet<ToolCallId> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } if is_graph_memory_tool(tool_name) || is_memory_write_tool(tool_name, input) => {
                Some(*tool_id)
            }
            _ => None,
        })
        .collect()
}

fn is_memory_write_tool(tool_name: &str, input: &serde_json::Value) -> bool {
    if matches!(tool_name, "memory_remember" | "memory_supersede") {
        return true;
    }

    matches!(
        tool_name,
        "file_write" | "str_replace" | "file_edit" | "write_file"
    ) && input_path(input).is_some_and(is_memory_path)
}

fn is_graph_memory_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "memory_remember" | "memory_supersede" | "memory_forget"
    )
}

fn input_path(input: &serde_json::Value) -> Option<&str> {
    input.get("path").and_then(serde_json::Value::as_str)
}

fn is_memory_path(path: &str) -> bool {
    let path = path.trim().trim_start_matches("./");
    path == "memory" || path.starts_with("memory/") || path.contains("/memory/")
}

/// In-memory `SessionStore` doubles for hermetic long-conversation unit tests.
#[cfg(test)]
pub(crate) mod test_session_store {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        error::Result, events::Event, traits::SessionStore, types::events_stream::EventFilter,
        types::events_stream::EventRange, types::events_stream::EventRecord,
        types::events_stream::SequenceNum, types::identifiers::SessionId,
        types::identifiers::TenantId, types::session::SessionFilter, types::session::SessionMeta,
        types::session::SessionStatus, types::session::SessionSummary,
    };

    /// Session store that returns a synthetic hit for configured planted facts and
    /// records the search limits it was queried with.
    ///
    /// `search_events` is the only method memory-recall scoring exercises; every
    /// other trait method is a benign no-op so the double stays small.
    #[derive(Clone, Default)]
    pub(crate) struct RecordingSessionStore {
        recalled_facts: HashSet<String>,
        observed_limits: Arc<Mutex<Vec<Option<usize>>>>,
    }

    impl RecordingSessionStore {
        /// Builds a store whose `search_events` returns one hit for each listed fact.
        pub(crate) fn with_recalled_facts<I, S>(recalled_facts: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            Self {
                recalled_facts: recalled_facts.into_iter().map(Into::into).collect(),
                observed_limits: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Returns the `limit` passed to every observed `search_events` call, in order.
        pub(crate) fn observed_limits(&self) -> Vec<Option<usize>> {
            self.observed_limits
                .lock()
                .expect("observed-limits lock should not be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl SessionStore for RecordingSessionStore {
        async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
            Ok(meta.id)
        }

        async fn emit_event(&self, _session_id: SessionId, _event: Event) -> Result<SequenceNum> {
            Ok(0)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            Ok(Vec::new())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(SessionMeta::default())
        }

        async fn update_status(
            &self,
            _session_id: SessionId,
            _status: SessionStatus,
        ) -> Result<()> {
            Ok(())
        }

        async fn search_events(
            &self,
            query: &str,
            filter: EventFilter,
        ) -> Result<Vec<EventRecord>> {
            self.observed_limits
                .lock()
                .expect("observed-limits lock should not be poisoned")
                .push(filter.limit);
            if !self.recalled_facts.contains(query) {
                return Ok(Vec::new());
            }
            let event = Event::Warning {
                message: query.to_string(),
            };
            Ok(vec![EventRecord {
                id: uuid::Uuid::now_v7(),
                session_id: filter.session_id.unwrap_or_default(),
                sequence_num: 0,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            }])
        }

        async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }

        async fn tenant_cost_since(
            &self,
            _tenant_id: &TenantId,
            _since: DateTime<Utc>,
        ) -> Result<u32> {
            Ok(0)
        }

        async fn delete_empty_session(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::types::tools::ToolOutput;
    use serde_json::json;

    use super::test_session_store::RecordingSessionStore;
    use super::*;

    fn tool_id(value: u128) -> ToolCallId {
        uuid::Uuid::from_u128(value).into()
    }

    fn tool_call(tool_id: ToolCallId, tool_name: &str, input: serde_json::Value) -> Event {
        Event::ToolCall {
            tool_id,
            provider_tool_use_id: None,
            provider_thought_signature: None,
            tool_name: tool_name.to_string(),
            input,
            hand_id: None,
        }
    }

    fn tool_result(tool_id: ToolCallId, success: bool) -> Event {
        Event::ToolResult {
            tool_id,
            provider_tool_use_id: None,
            output: ToolOutput::text("ok", Duration::ZERO),
            original_output_tokens: None,
            success,
            duration_ms: 1,
        }
    }

    #[test]
    fn pages_written_counts_successful_memory_tool_paths() {
        // Pins: long-conversation memory page metrics use real persisted tool events.
        let memory_file = tool_id(1);
        let normal_file = tool_id(2);
        let failed_memory = tool_id(3);
        let fast_memory = tool_id(4);
        let events = vec![
            tool_call(
                memory_file,
                "file_write",
                json!({ "path": "memory/auth.md" }),
            ),
            tool_result(memory_file, true),
            tool_call(normal_file, "file_write", json!({ "path": "src/lib.rs" })),
            tool_result(normal_file, true),
            tool_call(
                failed_memory,
                "file_write",
                json!({ "path": "memory/fail.md" }),
            ),
            tool_result(failed_memory, false),
            tool_call(
                fast_memory,
                "memory_remember",
                json!({ "items": [{ "text": "fact" }] }),
            ),
            tool_result(fast_memory, true),
        ];

        assert_eq!(count_pages_written(&events), 2);
    }

    #[test]
    fn consolidation_outcomes_count_graph_memory_tool_results() {
        // Pins: memory outcome metrics do not depend only on never-emitted MemoryIngest events.
        let remembered = tool_id(11);
        let failed = tool_id(12);
        let events = vec![
            tool_call(
                remembered,
                "memory_remember",
                json!({ "items": [{ "text": "fact" }] }),
            ),
            tool_result(remembered, true),
            tool_call(failed, "memory_forget", json!({ "name": "fact" })),
            Event::ToolError {
                tool_id: failed,
                provider_tool_use_id: None,
                tool_name: "memory_forget".to_string(),
                error: "boom".to_string(),
                retryable: false,
            },
        ];

        assert_eq!(
            count_consolidation_outcomes(&events),
            ConsolidationOutcomes {
                successes: 1,
                failures: 1,
                skipped: 0
            }
        );
    }

    #[tokio::test]
    async fn planted_fact_recall_is_zero_without_searching_for_an_empty_planted_set() {
        // Pins: an empty planted-fact set short-circuits to 0.0 and never queries the store.
        let store = RecordingSessionStore::with_recalled_facts(["unused fact"]);
        let scenario = MemoryScenario {
            planted_facts: Vec::new(),
            recall_k: 25,
        };

        let recall = compute_planted_fact_recall(&scenario, SessionId::new(), &store)
            .await
            .expect("empty planted set is infallible");

        assert_eq!(recall, 0.0);
        assert!(
            store.observed_limits().is_empty(),
            "empty planted set must not issue any session search"
        );
    }

    #[tokio::test]
    async fn planted_fact_recall_clamps_zero_recall_k_to_search_limit_one() {
        // Pins: recall_k=0 is clamped to a search limit of 1 instead of issuing a zero-limit query.
        let store = RecordingSessionStore::with_recalled_facts(["fact one", "fact two"]);
        let scenario = MemoryScenario {
            planted_facts: vec!["fact one".to_string(), "fact two".to_string()],
            recall_k: 0,
        };

        let recall = compute_planted_fact_recall(&scenario, SessionId::new(), &store)
            .await
            .expect("recall scoring is infallible against the in-memory store");

        assert_eq!(recall, 1.0);
        assert_eq!(
            store.observed_limits(),
            vec![Some(1), Some(1)],
            "recall_k=0 must clamp the per-fact search limit to 1"
        );
    }

    #[tokio::test]
    async fn planted_fact_recall_returns_the_exact_recalled_fraction() {
        // Pins: recall is recalled_facts / planted_facts using the configured recall_k as the search limit.
        let store = RecordingSessionStore::with_recalled_facts(["alpha", "gamma"]);
        let scenario = MemoryScenario {
            planted_facts: vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string(),
                "delta".to_string(),
            ],
            recall_k: 10,
        };

        let recall = compute_planted_fact_recall(&scenario, SessionId::new(), &store)
            .await
            .expect("recall scoring is infallible against the in-memory store");

        assert_eq!(recall, 0.5);
        assert_eq!(
            store.observed_limits(),
            vec![Some(10), Some(10), Some(10), Some(10)],
            "each planted fact must be searched once at the configured recall_k"
        );
    }
}
