//! LLM-driven tier 3 checkpoint summarization.

use moa_core::{
    CompactionConfig, ContextMessage, LLMProvider, ModelTask, Result, SessionStore, WorkingContext,
};

use crate::compaction::{latest_checkpoint_state, maybe_compact_events, non_checkpoint_events};
use crate::pipeline::history::preserved_error_messages;

use super::triggers::recent_turn_boundary_messages;

pub(super) struct Tier3Summary {
    pub(super) messages: Vec<ContextMessage>,
    pub(super) summary: String,
    pub(super) events_summarized: usize,
}

pub(super) async fn apply_tier3(
    ctx: &WorkingContext,
    history_messages: &[ContextMessage],
    config: &CompactionConfig,
    session_store: &dyn SessionStore,
    llm_provider: &dyn LLMProvider,
) -> Result<Option<Tier3Summary>> {
    let mut events = session_store
        .get_events(ctx.session_id, moa_core::EventRange::all())
        .await?;
    let mut forced_config = config.clone();
    forced_config.enabled = true;
    forced_config.event_threshold = 1;
    forced_config.token_ratio_threshold = 0.0;
    let Some(checkpoint_record) = maybe_compact_events(
        &forced_config,
        session_store,
        llm_provider,
        ModelTask::Summarization.tier(),
        ctx.session_id,
        ctx.model_capabilities.context_window,
        &events,
    )
    .await?
    else {
        return Ok(None);
    };
    // Fold the freshly emitted checkpoint into the already-loaded log instead of
    // re-reading the full event stream.
    events.push(checkpoint_record);

    let Some(checkpoint) = latest_checkpoint_state(&events) else {
        return Ok(None);
    };
    let non_checkpoint = non_checkpoint_events(&events);
    let summarized = checkpoint.events_summarized.min(non_checkpoint.len());
    let preserved_errors = preserved_error_messages(&non_checkpoint[..summarized]);
    let recent_boundary =
        recent_turn_boundary_messages(history_messages, config.recent_turns_verbatim);
    let recent_tail = history_messages[recent_boundary.min(history_messages.len())..].to_vec();

    let mut messages = preserved_errors;
    messages.push(ContextMessage::system(format!(
        "<session_checkpoint summarized_events=\"{}\">\n{}\n</session_checkpoint>",
        checkpoint.events_summarized, checkpoint.summary
    )));
    messages.extend(recent_tail);

    Ok(Some(Tier3Summary {
        messages,
        summary: checkpoint.summary,
        events_summarized: checkpoint.events_summarized,
    }))
}
