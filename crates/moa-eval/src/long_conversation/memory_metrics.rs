//! Memory-recall metrics for long-conversation eval runs.

use std::collections::HashSet;

use moa_core::{Event, EventFilter, EventType, SessionId, SessionStore, ToolCallId};

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

/// Returns an event-type filter for memory-recall queries.
#[must_use]
pub fn memory_event_types() -> Vec<EventType> {
    vec![
        EventType::MemoryRead,
        EventType::MemoryWrite,
        EventType::MemoryIngest,
        EventType::ToolCall,
        EventType::ToolResult,
        EventType::ToolError,
        EventType::BrainResponse,
    ]
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::ToolOutput;
    use serde_json::json;

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
            tool_call(fast_memory, "memory_remember", json!({ "text": "fact" })),
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
            tool_call(remembered, "memory_remember", json!({ "text": "fact" })),
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
}
