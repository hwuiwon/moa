//! Conversation input loading for query rewriting.

use moa_core::{
    error::Result, events::Event, types::context::ContextMessage, types::context::MessageRole,
    types::context::WorkingContext, types::events_stream::EventRange,
    types::events_stream::EventRecord,
};

use super::QueryRewriter;

const RECENT_HISTORY_EVENT_LIMIT: usize = 32;
const MAX_HISTORY_MESSAGES: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RewriteInput {
    pub(super) query: String,
    pub(super) history: Vec<ContextMessage>,
    pub(super) user_message_count: usize,
}

impl RewriteInput {
    pub(super) fn empty() -> Self {
        Self {
            query: String::new(),
            history: Vec::new(),
            user_message_count: 0,
        }
    }
}

impl QueryRewriter {
    pub(super) async fn load_input(&self, ctx: &WorkingContext) -> Result<Option<RewriteInput>> {
        if let Some(input) = input_from_context_messages(&ctx.messages) {
            return Ok(Some(input));
        }

        if !ctx.recent_events().is_empty() {
            return Ok(input_from_event_records(ctx.recent_events()));
        }

        let Some(session_store) = &self.session_store else {
            return Ok(None);
        };

        let records = session_store
            .get_events(
                ctx.session_id,
                EventRange::recent(RECENT_HISTORY_EVENT_LIMIT),
            )
            .await?;
        Ok(input_from_event_records(&records))
    }
}

fn input_from_context_messages(messages: &[ContextMessage]) -> Option<RewriteInput> {
    let conversation = messages
        .iter()
        .filter(|message| matches!(message.role, MessageRole::User | MessageRole::Assistant))
        .cloned()
        .collect::<Vec<_>>();
    input_from_conversation(conversation)
}

fn input_from_event_records(records: &[EventRecord]) -> Option<RewriteInput> {
    let conversation = records
        .iter()
        .filter_map(event_to_rewrite_message)
        .collect::<Vec<_>>();
    input_from_conversation(conversation)
}

fn event_to_rewrite_message(record: &EventRecord) -> Option<ContextMessage> {
    match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
            Some(ContextMessage::user(text.clone()))
        }
        Event::BrainResponse { text, .. } => Some(ContextMessage::assistant(text.clone())),
        _ => None,
    }
}

fn input_from_conversation(conversation: Vec<ContextMessage>) -> Option<RewriteInput> {
    let last_user_index = conversation
        .iter()
        .rposition(|message| message.role == MessageRole::User)?;
    let query = conversation.get(last_user_index)?.content.clone();
    let user_message_count = conversation
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count();
    let start = last_user_index.saturating_sub(MAX_HISTORY_MESSAGES);
    let history = conversation[start..last_user_index].to_vec();

    Some(RewriteInput {
        query,
        history,
        user_message_count,
    })
}
