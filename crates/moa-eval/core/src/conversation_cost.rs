//! Conversation-cost analyzer: reconstruct per-turn and per-conversation tool-call / token /
//! coordination KPIs from a session's durable event log.
//!
//! This is the single measurement substrate shared by the deterministic coordination tests and
//! the live sweep. Model-side KPIs (model turns, tool calls, tokens, cost, cache-hit ratio) are
//! reconstructed from [`Event::BrainResponse`]/[`Event::ToolCall`] and therefore work on ANY
//! session with no extra instrumentation. Coordination KPIs (internal virtual-object round-trips)
//! are summed from [`Event::TurnMetrics`] telemetry events, present only when
//! `MOA_PERSIST_TURN_METRICS` was enabled for the run (the deterministic tests enable it).

use std::collections::BTreeMap;

use moa_core::{
    coordination_counters::CoordinationSnapshot, events::Event, types::events_stream::EventRecord,
};

/// Per-model-turn cost (boundary = one `BrainResponse` event).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnCost {
    /// One-based model-turn number within the conversation.
    pub turn_number: u64,
    /// Model recorded for the turn.
    pub model: String,
    /// Tool calls dispatched during the turn.
    pub tool_call_count: u64,
    /// Uncached input tokens.
    pub input_tokens_uncached: u64,
    /// Cache-write input tokens.
    pub input_tokens_cache_write: u64,
    /// Cache-read input tokens.
    pub input_tokens_cache_read: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Turn cost in cents.
    pub cost_cents: u64,
}

impl TurnCost {
    /// Total input tokens (uncached + cache-write + cache-read).
    #[must_use]
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens_uncached
            .saturating_add(self.input_tokens_cache_write)
            .saturating_add(self.input_tokens_cache_read)
    }

    /// Fraction of input tokens served from cache (0.0 when the turn read no input tokens).
    #[must_use]
    pub fn cache_hit_ratio(&self) -> f64 {
        let total = self.total_input_tokens();
        if total == 0 {
            0.0
        } else {
            self.input_tokens_cache_read as f64 / total as f64
        }
    }
}

/// Full per-conversation cost reconstruction.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationCost {
    /// Number of model turns (count of `BrainResponse` events).
    pub model_turns: u64,
    /// Total tool calls dispatched (count of `ToolCall` events).
    pub total_tool_calls: u64,
    /// Tool calls broken down by tool name.
    pub tool_calls_by_name: BTreeMap<String, u64>,
    /// Total input tokens across the conversation.
    pub total_input_tokens: u64,
    /// Total output tokens across the conversation.
    pub total_output_tokens: u64,
    /// Total cost in cents.
    pub total_cost_cents: u64,
    /// Workers spawned (count of `WorkerSpawned` events).
    pub worker_spawns: u64,
    /// Result bundles emitted (count of `WorkerResultBundle` events).
    pub worker_result_bundles: u64,
    /// Total worker results across all bundles.
    pub bundled_results: u64,
    /// Durable error events (`Error` + `ToolError`).
    pub error_events: u64,
    /// Trimmed text of the LAST `BrainResponse` event (the conversation's final model reply), or
    /// `None` when no `BrainResponse` was recorded. `Some("")` distinguishes an empty final reply
    /// (a coordinator that returned nothing) from the absence of any model turn.
    pub final_text: Option<String>,
    /// Whether any `TurnMetrics` telemetry was found. When false, persistence was disabled for the
    /// run: [`Self::coordination`] and [`Self::get_events_calls`] are zero and only the model-side
    /// KPIs are meaningful.
    pub coordination_present: bool,
    /// Conversation-total internal coordination round-trips (Session/Worker VO calls, fire-and-forget
    /// sends, durable appends), summed from `TurnMetrics`. Reuses the production
    /// [`CoordinationSnapshot`] recorder type so the analyzer and the runtime recorder share one
    /// shape; only meaningful when [`Self::coordination_present`].
    pub coordination: CoordinationSnapshot,
    /// `get_events` replay reads across the conversation, summed from `TurnMetrics`. Kept alongside
    /// (not inside) [`Self::coordination`] because the runtime recorder snapshot does not track it.
    pub get_events_calls: u64,
    /// Per-model-turn breakdown.
    pub turns: Vec<TurnCost>,
}

impl ConversationCost {
    /// Fraction of input tokens served from cache across the whole conversation.
    #[must_use]
    pub fn cache_hit_ratio(&self) -> f64 {
        let cache_read: u64 = self
            .turns
            .iter()
            .map(|turn| turn.input_tokens_cache_read)
            .sum();
        if self.total_input_tokens == 0 {
            0.0
        } else {
            cache_read as f64 / self.total_input_tokens as f64
        }
    }

    /// Average model tool calls per model turn (0.0 when there were no model turns).
    #[must_use]
    pub fn tool_calls_per_turn(&self) -> f64 {
        if self.model_turns == 0 {
            0.0
        } else {
            self.total_tool_calls as f64 / self.model_turns as f64
        }
    }

    /// Reconstructs the conversation cost from a session's durable events (any order-preserving
    /// slice; typically the full `get_events` / `Session/progress` result).
    #[must_use]
    pub fn from_events(events: &[EventRecord]) -> Self {
        let mut cost = ConversationCost {
            model_turns: 0,
            total_tool_calls: 0,
            tool_calls_by_name: BTreeMap::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            worker_spawns: 0,
            worker_result_bundles: 0,
            bundled_results: 0,
            error_events: 0,
            final_text: None,
            coordination_present: false,
            coordination: CoordinationSnapshot::default(),
            get_events_calls: 0,
            turns: Vec::new(),
        };
        // Tool calls dispatched since the last BrainResponse — attributed to the turn that
        // BrainResponse closes.
        let mut pending_tool_calls: u64 = 0;

        for record in events {
            match &record.event {
                Event::ToolCall { tool_name, .. } => {
                    cost.total_tool_calls += 1;
                    pending_tool_calls += 1;
                    *cost
                        .tool_calls_by_name
                        .entry(tool_name.clone())
                        .or_insert(0) += 1;
                }
                Event::BrainResponse {
                    text,
                    model,
                    input_tokens_uncached,
                    input_tokens_cache_write,
                    input_tokens_cache_read,
                    output_tokens,
                    cost_cents,
                    ..
                } => {
                    cost.model_turns += 1;
                    // Overwrite each turn so the last BrainResponse wins — the conversation's final
                    // model reply, which the coordination lanes assert is non-empty and free of raw
                    // worker output.
                    cost.final_text = Some(text.trim().to_string());
                    let uncached = *input_tokens_uncached as u64;
                    let cache_write = *input_tokens_cache_write as u64;
                    let cache_read = *input_tokens_cache_read as u64;
                    let output = *output_tokens as u64;
                    let turn_cost_cents = u64::from(*cost_cents);
                    cost.total_input_tokens += uncached
                        .saturating_add(cache_write)
                        .saturating_add(cache_read);
                    cost.total_output_tokens += output;
                    cost.total_cost_cents += turn_cost_cents;
                    cost.turns.push(TurnCost {
                        turn_number: cost.model_turns,
                        model: model.as_str().to_string(),
                        tool_call_count: pending_tool_calls,
                        input_tokens_uncached: uncached,
                        input_tokens_cache_write: cache_write,
                        input_tokens_cache_read: cache_read,
                        output_tokens: output,
                        cost_cents: turn_cost_cents,
                    });
                    pending_tool_calls = 0;
                }
                Event::WorkerSpawned { .. } => cost.worker_spawns += 1,
                Event::WorkerResultBundle { results, .. } => {
                    cost.worker_result_bundles += 1;
                    cost.bundled_results += results.len() as u64;
                }
                Event::Error { .. } | Event::ToolError { .. } => cost.error_events += 1,
                Event::TurnMetrics {
                    session_vo_calls,
                    worker_vo_calls,
                    vo_sends,
                    durable_appends,
                    get_events_calls,
                    ..
                } => {
                    cost.coordination_present = true;
                    cost.coordination.session_vo_calls += *session_vo_calls;
                    cost.coordination.worker_vo_calls += *worker_vo_calls;
                    cost.coordination.vo_sends += *vo_sends;
                    cost.coordination.durable_appends += *durable_appends;
                    cost.get_events_calls += *get_events_calls;
                }
                _ => {}
            }
        }
        cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use moa_core::{
        events::EventType, types::identifiers::ModelId, types::identifiers::SessionId,
        types::identifiers::ToolCallId, types::provider::ModelTier,
    };
    use uuid::Uuid;

    fn record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num,
            event_type: EventType::from(&event),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn brain_response(uncached: usize, cache_read: usize, output: usize, cost: u32) -> Event {
        Event::BrainResponse {
            text: "ok".to_string(),
            model: ModelId::new("gpt-5.4-mini"),
            model_tier: ModelTier::Main,
            input_tokens_uncached: uncached,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: cache_read,
            output_tokens: output,
            cost_cents: cost,
            duration_ms: 10,
            llm_ttft_ms: None,
            thought_signature: None,
        }
    }

    fn brain_response_with_text(text: &str) -> Event {
        Event::BrainResponse {
            text: text.to_string(),
            model: ModelId::new("gpt-5.4-mini"),
            model_tier: ModelTier::Main,
            input_tokens_uncached: 10,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 5,
            cost_cents: 1,
            duration_ms: 10,
            llm_ttft_ms: None,
            thought_signature: None,
        }
    }

    fn tool_call(name: &str) -> Event {
        Event::ToolCall {
            tool_id: ToolCallId::new(),
            provider_tool_use_id: None,
            provider_thought_signature: None,
            tool_name: name.to_string(),
            input: serde_json::json!({}),
            hand_id: None,
        }
    }

    #[test]
    fn reconstructs_per_turn_and_conversation_kpis_from_events() {
        // Pins: model turns, per-turn tool counts, tokens/cost, cache ratio, and coordination
        // totals are reconstructed from the durable event log.
        let session = SessionId::new();
        let events = vec![
            record(session, 1, tool_call("session_search")),
            record(session, 2, tool_call("session_search")),
            record(session, 3, brain_response(100, 900, 50, 3)), // turn 1: 2 tool calls
            record(
                session,
                4,
                Event::TurnMetrics {
                    turn_id: "t1".to_string(),
                    actor: "coordinator".to_string(),
                    session_vo_calls: 3,
                    worker_vo_calls: 2,
                    vo_sends: 1,
                    durable_appends: 4,
                    get_events_calls: 2,
                    events_bytes: 512,
                    llm_ms: 10,
                    tool_ms: 5,
                    persist_ms: 2,
                },
            ),
            record(session, 5, tool_call("file_read")),
            record(session, 6, brain_response(50, 950, 20, 1)), // turn 2: 1 tool call
        ];

        let cost = ConversationCost::from_events(&events);
        assert_eq!(cost.model_turns, 2);
        assert_eq!(cost.total_tool_calls, 3);
        assert_eq!(cost.tool_calls_by_name.get("session_search"), Some(&2));
        assert_eq!(cost.tool_calls_by_name.get("file_read"), Some(&1));
        assert_eq!(cost.total_output_tokens, 70);
        assert_eq!(cost.total_cost_cents, 4);
        assert_eq!(cost.turns.len(), 2);
        assert_eq!(cost.turns[0].tool_call_count, 2);
        assert_eq!(cost.turns[1].tool_call_count, 1);
        // Coordination totals from the single TurnMetrics event.
        assert!(cost.coordination_present);
        assert_eq!(cost.coordination.total_vo_calls(), 5);
        assert_eq!(cost.coordination.session_vo_calls, 3);
        assert_eq!(cost.coordination.durable_appends, 4);
        assert_eq!(cost.get_events_calls, 2);
        // Cache-hit ratio: 1850 cache-read of 2000 total input (1000 per turn).
        assert_eq!(cost.total_input_tokens, 2000);
        assert!((cost.cache_hit_ratio() - (1850.0 / 2000.0)).abs() < 1e-9);
        assert!((cost.tool_calls_per_turn() - 1.5).abs() < 1e-9);
        // The final reply is the text of the last BrainResponse.
        assert_eq!(cost.final_text.as_deref(), Some("ok"));
    }

    #[test]
    fn final_text_is_last_brain_response_trimmed_and_none_without_any() {
        // Pins: final_text captures the trimmed text of the LAST BrainResponse, and stays None when
        // no BrainResponse was recorded (so an empty final and an absent final are distinguishable).
        let session = SessionId::new();

        let no_final =
            ConversationCost::from_events(&[record(session, 1, tool_call("session_search"))]);
        assert_eq!(no_final.final_text, None);
        assert_eq!(no_final.model_turns, 0);

        let events = vec![
            record(session, 1, brain_response_with_text("  first draft  ")),
            record(
                session,
                2,
                brain_response_with_text("  Final synthesized answer.\n"),
            ),
        ];
        let cost = ConversationCost::from_events(&events);
        assert_eq!(
            cost.final_text.as_deref(),
            Some("Final synthesized answer.")
        );

        // An empty final reply is Some(""), not None — that is the empty-final failure signal.
        let empty_final =
            ConversationCost::from_events(&[record(session, 1, brain_response_with_text("   "))]);
        assert_eq!(empty_final.final_text.as_deref(), Some(""));
    }

    #[test]
    fn coordination_absent_when_no_turn_metrics() {
        let session = SessionId::new();
        let events = vec![record(session, 1, brain_response(10, 0, 5, 1))];
        let cost = ConversationCost::from_events(&events);
        assert_eq!(cost.model_turns, 1);
        assert!(!cost.coordination_present);
        assert_eq!(cost.coordination.total_vo_calls(), 0);
        assert_eq!(cost.get_events_calls, 0);
    }
}
