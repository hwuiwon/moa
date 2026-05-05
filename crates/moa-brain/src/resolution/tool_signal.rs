//! Tool-outcome signal for task-resolution scoring.

use std::collections::HashMap;

use moa_core::{Event, EventRecord, ToolCallId};

/// Scores tool outcomes for one segment.
#[must_use]
pub fn score(events: &[EventRecord]) -> Option<f64> {
    let outcomes = tool_outcomes(events);
    if outcomes.is_empty() {
        return Some(0.5);
    }

    let successes = outcomes.iter().filter(|outcome| outcome.success).count();
    let failures = outcomes.len().saturating_sub(successes);
    let last_success = outcomes.last().is_some_and(|outcome| outcome.success);

    if successes == outcomes.len() {
        Some(0.8)
    } else if failures == outcomes.len() {
        Some(0.1)
    } else if !last_success {
        Some(0.2)
    } else if successes > failures {
        Some(0.7)
    } else {
        Some(0.5)
    }
}

/// Returns whether every completed tool call in the segment failed.
#[must_use]
pub fn all_tools_failed(events: &[EventRecord]) -> bool {
    let outcomes = tool_outcomes(events);
    !outcomes.is_empty() && outcomes.iter().all(|outcome| !outcome.success)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutcome {
    success: bool,
}

fn tool_outcomes(events: &[EventRecord]) -> Vec<ToolOutcome> {
    let call_order = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_id, .. } => Some((*tool_id, record.sequence_num)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut statuses: HashMap<ToolCallId, bool> = HashMap::new();

    for record in events {
        match &record.event {
            Event::ToolResult {
                tool_id,
                output,
                success,
                ..
            } => {
                statuses.insert(*tool_id, *success && !output.is_error);
            }
            Event::ToolError { tool_id, .. } => {
                statuses.insert(*tool_id, false);
            }
            _ => {}
        }
    }

    let mut outcomes = call_order
        .into_iter()
        .filter_map(|(tool_id, sequence_num)| {
            statuses
                .get(&tool_id)
                .copied()
                .map(|success| (sequence_num, ToolOutcome { success }))
        })
        .collect::<Vec<_>>();
    outcomes.sort_by_key(|(sequence_num, _)| *sequence_num);
    outcomes
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use moa_core::{Event, EventRecord, EventType, ModelId, ModelTier, SessionId, ToolCallId};
    use serde_json::json;
    use uuid::Uuid;

    use super::{all_tools_failed, score};

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn tool_call(sequence_num: u64, tool_id: ToolCallId) -> EventRecord {
        record(
            sequence_num,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: json!({ "cmd": "cargo test" }),
                hand_id: None,
            },
        )
    }

    fn tool_result(sequence_num: u64, tool_id: ToolCallId, success: bool) -> EventRecord {
        record(
            sequence_num,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: moa_core::ToolOutput::from_process(
                    String::new(),
                    String::new(),
                    if success { 0 } else { 1 },
                    Duration::from_millis(10),
                ),
                original_output_tokens: None,
                success,
                duration_ms: 10,
            },
        )
    }

    fn brain_response(sequence_num: u64) -> EventRecord {
        record(
            sequence_num,
            Event::BrainResponse {
                text: "Done".to_string(),
                thought_signature: None,
                model: ModelId::new("test"),
                model_tier: ModelTier::Main,
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 0,
            },
        )
    }

    #[test]
    fn all_success_scores_high() {
        let first = ToolCallId::new();
        let second = ToolCallId::new();
        let events = vec![
            tool_call(0, first),
            tool_result(1, first, true),
            tool_call(2, second),
            tool_result(3, second, true),
        ];

        assert_eq!(score(&events), Some(0.8));
    }

    #[test]
    fn all_fail_scores_low() {
        let first = ToolCallId::new();
        let second = ToolCallId::new();
        let events = vec![
            tool_call(0, first),
            tool_result(1, first, false),
            tool_call(2, second),
            tool_result(3, second, false),
        ];

        assert_eq!(score(&events), Some(0.1));
        assert!(all_tools_failed(&events));
    }

    #[test]
    fn mixed_success_scores_neutral_when_recovery_is_unclear() {
        let first = ToolCallId::new();
        let second = ToolCallId::new();
        let events = vec![
            brain_response(0),
            tool_call(1, first),
            tool_result(2, first, false),
            tool_call(3, second),
            tool_result(4, second, true),
        ];

        assert_eq!(score(&events), Some(0.5));
    }

    #[test]
    fn last_tool_failed_scores_low() {
        let first = ToolCallId::new();
        let second = ToolCallId::new();
        let events = vec![
            tool_call(0, first),
            tool_result(1, first, true),
            tool_call(2, second),
            tool_result(3, second, false),
        ];

        assert_eq!(score(&events), Some(0.2));
    }

    #[test]
    fn no_tools_is_neutral() {
        assert_eq!(score(&[]), Some(0.5));
        let _ = EventType::ToolCall;
    }
}
