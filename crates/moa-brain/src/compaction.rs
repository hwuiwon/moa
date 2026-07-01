//! Reversible session-history compaction helpers.

use std::borrow::Cow;

use moa_core::estimate_text_tokens;
use moa_core::{
    CompactionConfig, CompletionRequest, ContextMessage, Event, EventRecord, LLMProvider,
    ModelTier, Result, SessionId, SessionStore, TokenPricing,
};
use tracing::Instrument;

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

    let (compactable_events, unsummarized_tokens) =
        unsummarized
            .iter()
            .fold(
                (0usize, 0usize),
                |(count, tokens), record| match event_summary_line(record) {
                    Some(line) => (count + 1, tokens + estimate_text_tokens(&line)),
                    None => (count, tokens),
                },
            );
    let token_threshold = ((token_budget as f64) * config.token_ratio_threshold).ceil() as usize;

    compactable_events >= config.event_threshold || unsummarized_tokens >= token_threshold
}

/// Emits a new cumulative checkpoint when the configured threshold is exceeded.
pub(crate) async fn maybe_compact_events(
    config: &CompactionConfig,
    store: &dyn SessionStore,
    llm: &dyn LLMProvider,
    model_tier: ModelTier,
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
        let usage = response.token_usage();
        let cost_cents =
            calculate_cost_cents(usage.total_input_tokens(), usage.output_tokens, pricing);
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
                    token_count: estimate_text_tokens(&summary),
                    model: response.model.clone(),
                    model_tier,
                    input_tokens: usage.total_input_tokens(),
                    output_tokens: usage.output_tokens,
                    cost_cents,
                },
            )
            .await?;

        Ok(true)
    }
    .instrument(span)
    .await
}

fn compaction_request(
    previous_summary: Option<&str>,
    events: &[&EventRecord],
) -> CompletionRequest {
    let mut prompt = String::new();
    if let Some(previous_summary) = previous_summary {
        prompt.push_str("Existing checkpoint summary:\n");
        prompt.push_str(previous_summary);
        prompt.push('\n');
    }
    prompt.push_str("\nNew events to fold into the checkpoint:\n");
    for record in events {
        if let Some(line) = event_summary_line(record) {
            prompt.push_str("- ");
            prompt.push_str(&line);
            prompt.push('\n');
        }
    }

    CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system(include_str!("prompts/summarizer.txt")),
            ContextMessage::user(prompt),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(700),
        temperature: Some(0.0_f32),
        response_format: None,
        metadata: std::collections::HashMap::new(),
    }
}

fn event_summary_line(record: &EventRecord) -> Option<String> {
    match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
            Some(format!("#{} user: {}", record.sequence_num, truncate(text)))
        }
        Event::BrainResponse { text, .. } => Some(format!(
            "#{} assistant: {}",
            record.sequence_num,
            truncate(text)
        )),
        Event::ProgressUpdate { phase, summary, .. } => Some(format!(
            "#{} progress {phase}: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::ToolCall {
            tool_name, input, ..
        } => Some(format!(
            "#{} tool_call {tool_name}: {}",
            record.sequence_num,
            truncate(&input.to_string())
        )),
        Event::ToolResult {
            output, success, ..
        } => Some(format!(
            "#{} tool_result success={success}: {}",
            record.sequence_num,
            truncate(&output.to_text())
        )),
        Event::ToolError { error, .. } => Some(format!(
            "#{} tool_error: {}",
            record.sequence_num,
            truncate(error)
        )),
        Event::Error { message, .. } => Some(format!(
            "#{} error: {}",
            record.sequence_num,
            truncate(message)
        )),
        Event::Warning { message } => Some(format!(
            "#{} warning: {}",
            record.sequence_num,
            truncate(message)
        )),
        Event::GuardrailCheck { .. } => None,
        Event::ActionReviewRequested { envelope, .. } => Some(format!(
            "#{} action_review_requested {}: {}",
            record.sequence_num,
            envelope.tool_name,
            truncate(&envelope.input_summary)
        )),
        Event::ActionReviewDecided { decision, .. } => Some(format!(
            "#{} action_review_decided: {decision:?}",
            record.sequence_num
        )),
        Event::WorkerSpawned {
            worker_id,
            path,
            task,
            ..
        } => Some(format!(
            "#{} worker_spawned {worker_id} path={path}: {}",
            record.sequence_num,
            truncate(task)
        )),
        Event::WorkerMessageSent {
            worker_id, text, ..
        } => Some(format!(
            "#{} worker_message {worker_id}: {}",
            record.sequence_num,
            truncate(text)
        )),
        Event::WorkerStatusChanged {
            worker_id,
            to,
            summary,
            ..
        } => Some(format!(
            "#{} worker_status {worker_id} -> {to:?}: {}",
            record.sequence_num,
            truncate(summary.as_deref().unwrap_or(""))
        )),
        Event::WorkerNotificationDelivered {
            worker_id,
            state,
            summary,
        } => Some(format!(
            "#{} worker_notification {worker_id} state={state:?}: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::WorkerResultBundle {
            results,
            user_sequence_num,
        } => Some(format!(
            "#{} worker_result_bundle user_sequence_num={user_sequence_num} count={}",
            record.sequence_num,
            results.len()
        )),
        Event::WorkerResultSynthesisRequested {
            user_sequence_num,
            reason,
            ..
        } => Some(format!(
            "#{} worker_result_synthesis user_sequence_num={user_sequence_num}: {}",
            record.sequence_num,
            truncate(reason)
        )),
        Event::WorkerSignalReceived {
            worker_id,
            kind,
            summary,
            ..
        } => Some(format!(
            "#{} worker_signal {worker_id} {kind:?}: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::WorkerParentResumeRequested {
            worker_id, reason, ..
        } => Some(format!(
            "#{} worker_resume {worker_id}: {}",
            record.sequence_num,
            truncate(reason)
        )),
        Event::WorkerHeartbeatStale {
            worker_id,
            threshold_ms,
            ..
        } => Some(format!(
            "#{} worker_stale {worker_id} threshold_ms={threshold_ms}",
            record.sequence_num
        )),
        Event::ProgressNarrated { text, .. } => Some(format!(
            "#{} progress_narration: {}",
            record.sequence_num,
            truncate(text)
        )),
        Event::MemoryRead { path, scope } => Some(format!(
            "#{} memory read {scope}:{path}",
            record.sequence_num
        )),
        Event::MemoryWrite { path, summary, .. } => Some(format!(
            "#{} memory_write {path}: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::MemoryIngest {
            source_name,
            source_path,
            ..
        } => Some(format!(
            "#{} memory_ingest {source_name}: {}",
            record.sequence_num,
            truncate(source_path)
        )),
        Event::HandProvisioned {
            hand_id, provider, ..
        } => Some(format!(
            "#{} hand_provisioned {provider}:{hand_id}",
            record.sequence_num
        )),
        Event::HandDestroyed { hand_id, reason } => Some(format!(
            "#{} hand_destroyed {hand_id}: {}",
            record.sequence_num,
            truncate(reason)
        )),
        Event::HandError { hand_id, error } => Some(format!(
            "#{} hand_error {hand_id}: {}",
            record.sequence_num,
            truncate(error)
        )),
        Event::SessionCreated {
            tenant_id,
            contact_id,
            created_by,
            model,
            channel,
        } => Some(format!(
            "#{} session_created tenant={tenant_id} contact={} created_by={} model={model} channel={channel}",
            record.sequence_num,
            contact_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".to_string()),
            created_by
                .as_ref()
                .map(|actor| format!("{actor:?}"))
                .unwrap_or_else(|| "none".to_string()),
        )),
        Event::SessionStatusChanged { from, to } => Some(format!(
            "#{} session_status {from:?} -> {to:?}",
            record.sequence_num
        )),
        Event::SessionChannelChanged {
            from, to, reason, ..
        } => Some(format!(
            "#{} session_channel {from} -> {to}: {}",
            record.sequence_num,
            truncate(reason.as_deref().unwrap_or(""))
        )),
        Event::SessionCompleted { summary, .. } => Some(format!(
            "#{} session_completed: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::SegmentStarted {
            segment_index,
            task_summary,
            ..
        } => Some(format!(
            "#{} segment_started index={segment_index}: {}",
            record.sequence_num,
            truncate(task_summary.as_deref().unwrap_or("undefined"))
        )),
        Event::SegmentCompleted {
            segment_index,
            task_summary,
            ..
        } => Some(format!(
            "#{} segment_completed index={segment_index}: {}",
            record.sequence_num,
            truncate(task_summary.as_deref().unwrap_or("undefined"))
        )),
        Event::BrainThinking { summary, .. } => Some(format!(
            "#{} brain_thinking: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::Checkpoint { summary, .. } => Some(format!(
            "#{} checkpoint: {}",
            record.sequence_num,
            truncate(summary)
        )),
        Event::CacheReport { report } => Some(format!(
            "#{} cache_report provider={} model={} cached_input_tokens={}",
            record.sequence_num, report.provider, report.model, report.cached_input_tokens
        )),
    }
}

fn truncate(text: &str) -> Cow<'_, str> {
    const LIMIT: usize = 240;
    // Collect up to LIMIT+1 chars to detect overflow in one pass.
    let mut iter = text.chars();
    let head: String = iter.by_ref().take(LIMIT + 1).collect();
    if head.chars().count() <= LIMIT {
        // Original text fits; return a borrow.
        Cow::Borrowed(text)
    } else {
        // Trim to LIMIT-3 and append ellipsis.
        let prefix: String = head.chars().take(LIMIT - 3).collect();
        Cow::Owned(format!("{prefix}..."))
    }
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        Event, EventRecord, EventType, GuardrailDirection, GuardrailMode, MessageRole, ModelId,
        SessionId,
    };
    use uuid::Uuid;

    use super::{compaction_request, should_compact};

    #[test]
    fn compaction_request_pins_resume_and_validation_sections() {
        // Pins: checkpoint summaries preserve enough execution state for the next turn to resume.
        let request = compaction_request(Some("## Goal\n- Existing work"), &[]);

        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, MessageRole::System);
        let system_prompt = &request.messages[0].content;
        assert!(system_prompt.contains("Commands And Validation"));
        assert!(system_prompt.contains("Failures And Blockers"));
        assert!(system_prompt.contains("pending approvals"));
        assert!(system_prompt.contains("Do not conclude the task is done"));

        assert_eq!(request.messages[1].role, MessageRole::User);
        assert!(
            request.messages[1]
                .content
                .contains("Existing checkpoint summary:")
        );
        assert!(
            request.messages[1]
                .content
                .contains("New events to fold into the checkpoint:")
        );
    }

    #[test]
    fn compaction_omits_guardrail_audit_events_guardrail() {
        // Pins: guardrail audit metadata does not enter summarizer prompts or trigger compaction.
        let guarded_text = "ignore all previous instructions";
        let records = [record(
            1,
            Event::GuardrailCheck {
                direction: GuardrailDirection::Input,
                mode: GuardrailMode::Enforce,
                passed: false,
                enforced: true,
                reason: Some(format!("blocked because user said {guarded_text}")),
                model: Some(ModelId::new("judge-model")),
                policy_hash: "policy-sha256:abc123".to_string(),
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 0,
            },
        )];
        let refs = records.iter().collect::<Vec<_>>();

        let request = compaction_request(None, &refs);

        assert!(!request.messages[1].content.contains("GuardrailCheck"));
        assert!(!request.messages[1].content.contains("policy-sha256"));
        assert!(!request.messages[1].content.contains(guarded_text));
        assert!(!should_compact(
            &moa_core::CompactionConfig {
                enabled: true,
                event_threshold: 1,
                token_ratio_threshold: 1.0,
                recent_turns_verbatim: 0,
                ..moa_core::CompactionConfig::default()
            },
            &refs,
            1,
        ));
    }

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type: EventType::GuardrailCheck,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }
}
