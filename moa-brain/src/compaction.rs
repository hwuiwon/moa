//! Reversible session-history compaction helpers.

use moa_core::{
    CompactionConfig, CompletionRequest, ContextMessage, Event, EventRange, EventRecord,
    LLMProvider, Result, SessionId, SessionStore, TokenPricing,
};
use tracing::Instrument;

use crate::pipeline::ContextPipeline;
use crate::pipeline::estimate_tokens;

/// Latest checkpoint summary state derived from the append-only event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckpointState {
    /// Summary text stored in the most recent checkpoint event.
    pub summary: String,
    /// Number of non-checkpoint events summarized by the checkpoint.
    pub events_summarized: usize,
}

/// Returns the latest checkpoint state, if one exists.
pub(crate) fn latest_checkpoint_state(events: &[EventRecord]) -> Option<CheckpointState> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::Checkpoint {
            summary,
            events_summarized,
            ..
        } => Some(CheckpointState {
            summary: summary.clone(),
            events_summarized: (*events_summarized) as usize,
        }),
        _ => None,
    })
}

/// Returns all non-checkpoint events in original order.
pub(crate) fn non_checkpoint_events(events: &[EventRecord]) -> Vec<&EventRecord> {
    events
        .iter()
        .filter(|record| !matches!(record.event, Event::Checkpoint { .. }))
        .collect()
}

/// Returns the unsummarized non-checkpoint tail in original order.
pub(crate) fn unsummarized_events(events: &[EventRecord]) -> Vec<&EventRecord> {
    let all = non_checkpoint_events(events);
    let summarized = latest_checkpoint_state(events)
        .map(|checkpoint| checkpoint.events_summarized)
        .unwrap_or(0);
    all.into_iter().skip(summarized).collect()
}

/// Returns the index where the last `recent_turns` user-authored turns begin.
pub(crate) fn recent_turn_boundary(events: &[&EventRecord], recent_turns: usize) -> usize {
    if recent_turns == 0 || events.is_empty() {
        return events.len();
    }

    let mut turns_seen = 0usize;
    for index in (0..events.len()).rev() {
        if matches!(
            events[index].event,
            Event::UserMessage { .. } | Event::QueuedMessage { .. }
        ) {
            turns_seen += 1;
            if turns_seen == recent_turns {
                return index;
            }
        }
    }

    0
}

/// Returns whether the unsummarized tail is large enough to justify compaction.
pub(crate) fn should_compact(
    config: &CompactionConfig,
    unsummarized: &[&EventRecord],
    token_budget: usize,
) -> bool {
    if !config.enabled || unsummarized.is_empty() {
        return false;
    }

    let unsummarized_tokens = unsummarized
        .iter()
        .map(|record| estimate_tokens(&event_summary_line(record)))
        .sum::<usize>();
    let token_threshold = ((token_budget as f64) * config.token_ratio_threshold).ceil() as usize;

    unsummarized.len() >= config.event_threshold || unsummarized_tokens >= token_threshold
}

/// Emits a new cumulative checkpoint when the configured threshold is exceeded.
pub(crate) async fn maybe_compact_events(
    config: &CompactionConfig,
    store: &dyn SessionStore,
    llm: &dyn LLMProvider,
    session_id: SessionId,
    token_budget: usize,
    events: &[EventRecord],
) -> Result<bool> {
    let span = tracing::info_span!("compaction", moa.session.id = %session_id);
    async move {
        let unsummarized = unsummarized_events(events);
        if !should_compact(config, &unsummarized, token_budget) {
            return Ok(false);
        }

        let candidate_end = recent_turn_boundary(&unsummarized, config.recent_turns_verbatim);
        if candidate_end == 0 {
            return Ok(false);
        }

        let checkpoint = latest_checkpoint_state(events);
        let candidate = &unsummarized[..candidate_end];
        let response = llm
            .complete(compaction_request(
                checkpoint.as_ref().map(|state| state.summary.as_str()),
                candidate,
            ))
            .await?
            .collect()
            .await?;
        let summary = normalize_summary(&response.text);
        let pricing = &llm.capabilities().pricing;
        let cost_cents =
            calculate_cost_cents(response.input_tokens, response.output_tokens, pricing);
        let summarized_events = checkpoint
            .as_ref()
            .map(|state| state.events_summarized)
            .unwrap_or(0)
            + candidate.len();

        store
            .emit_event(
                session_id,
                Event::Checkpoint {
                    summary: summary.clone(),
                    events_summarized: summarized_events as u64,
                    token_count: estimate_tokens(&summary),
                    model: llm.capabilities().model_id.clone(),
                    input_tokens: response.input_tokens,
                    output_tokens: response.output_tokens,
                    cost_cents,
                },
            )
            .await?;

        Ok(true)
    }
    .instrument(span)
    .await
}

/// Backward-compatible entry point retained for existing exports.
pub async fn maybe_compact(
    store: &dyn SessionStore,
    llm: &dyn LLMProvider,
    session_id: SessionId,
    _pipeline: &ContextPipeline,
) -> Result<bool> {
    let events = store
        .get_events(session_id.clone(), EventRange::all())
        .await?;
    maybe_compact_events(
        &CompactionConfig::default(),
        store,
        llm,
        session_id,
        llm.capabilities().context_window,
        &events,
    )
    .await
}

fn compaction_request(
    previous_summary: Option<&str>,
    events: &[&EventRecord],
) -> CompletionRequest {
    let mut prompt = String::from(
        "Create a reversible checkpoint summary for an agent session.\n\
         Preserve concrete facts, file paths, commands, errors, fixes, decisions, and unresolved work.\n\
         Format the output as markdown with these headings:\n\
         - Key Facts\n\
         - Decisions\n\
         - Errors And Fixes\n\
         - Open Threads\n\
         - Active Files\n",
    );
    if let Some(previous_summary) = previous_summary {
        prompt.push_str("\nExisting checkpoint summary:\n");
        prompt.push_str(previous_summary);
        prompt.push('\n');
    }
    prompt.push_str("\nNew events to fold into the checkpoint:\n");
    for record in events {
        prompt.push_str("- ");
        prompt.push_str(&event_summary_line(record));
        prompt.push('\n');
    }

    CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system(
                "You compact agent session history without losing factual recoverability.",
            ),
            ContextMessage::user(prompt),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(700),
        temperature: Some(0.0_f32),
        cache_breakpoints: Vec::new(),
        metadata: std::collections::HashMap::new(),
    }
}

fn event_summary_line(record: &EventRecord) -> String {
    match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
            format!("#{} user: {}", record.sequence_num, truncate(text))
        }
        Event::BrainResponse { text, .. } => {
            format!("#{} assistant: {}", record.sequence_num, truncate(text))
        }
        Event::ToolCall {
            tool_name, input, ..
        } => format!(
            "#{} tool_call {tool_name}: {}",
            record.sequence_num,
            truncate(&input.to_string())
        ),
        Event::ToolResult {
            output, success, ..
        } => format!(
            "#{} tool_result success={success}: {}",
            record.sequence_num,
            truncate(&output.to_text())
        ),
        Event::ToolError { error, .. } => {
            format!("#{} tool_error: {}", record.sequence_num, truncate(error))
        }
        Event::Error { message, .. } => {
            format!("#{} error: {}", record.sequence_num, truncate(message))
        }
        Event::Warning { message } => {
            format!("#{} warning: {}", record.sequence_num, truncate(message))
        }
        Event::ApprovalRequested {
            tool_name,
            input_summary,
            ..
        } => format!(
            "#{} approval_requested {tool_name}: {}",
            record.sequence_num,
            truncate(input_summary)
        ),
        Event::ApprovalDecided { decision, .. } => {
            format!("#{} approval_decided: {decision:?}", record.sequence_num)
        }
        Event::MemoryRead { path, scope } => {
            format!("#{} memory_read {scope}:{path}", record.sequence_num)
        }
        Event::MemoryWrite { path, summary, .. } => format!(
            "#{} memory_write {path}: {}",
            record.sequence_num,
            truncate(summary)
        ),
        Event::MemoryIngest {
            source_name,
            source_path,
            ..
        } => format!(
            "#{} memory_ingest {source_name}: {}",
            record.sequence_num,
            truncate(source_path)
        ),
        Event::HandProvisioned {
            hand_id, provider, ..
        } => format!(
            "#{} hand_provisioned {provider}:{hand_id}",
            record.sequence_num
        ),
        Event::HandDestroyed { hand_id, reason } => format!(
            "#{} hand_destroyed {hand_id}: {}",
            record.sequence_num,
            truncate(reason)
        ),
        Event::HandError { hand_id, error } => format!(
            "#{} hand_error {hand_id}: {}",
            record.sequence_num,
            truncate(error)
        ),
        Event::SessionCreated {
            workspace_id,
            user_id,
            model,
        } => format!(
            "#{} session_created workspace={workspace_id} user={user_id} model={model}",
            record.sequence_num
        ),
        Event::SessionStatusChanged { from, to } => {
            format!("#{} session_status {from:?} -> {to:?}", record.sequence_num)
        }
        Event::SessionCompleted { summary, .. } => format!(
            "#{} session_completed: {}",
            record.sequence_num,
            truncate(summary)
        ),
        Event::BrainThinking { summary, .. } => format!(
            "#{} brain_thinking: {}",
            record.sequence_num,
            truncate(summary)
        ),
        Event::Checkpoint { summary, .. } => {
            format!("#{} checkpoint: {}", record.sequence_num, truncate(summary))
        }
    }
}

fn truncate(text: &str) -> String {
    const LIMIT: usize = 240;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }

    let prefix = text.chars().take(LIMIT - 3).collect::<String>();
    format!("{prefix}...")
}

fn normalize_summary(summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        "## Key Facts\n- No durable facts extracted.\n".to_string()
    } else {
        trimmed.to_string()
    }
}

fn calculate_cost_cents(input_tokens: usize, output_tokens: usize, pricing: &TokenPricing) -> u32 {
    let cost_dollars = ((input_tokens as f64 * pricing.input_per_mtok)
        + (output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0;
    (cost_dollars * 100.0).round() as u32
}
