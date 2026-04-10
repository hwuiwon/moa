//! Stage 6: compiles session history into context messages.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ContextMessage, ContextProcessor, Event, EventRange, EventRecord, ProcessorOutput, Result,
    SessionStore, WorkingContext,
};

use super::estimate_tokens;

const RECENT_MESSAGE_LIMIT: usize = 10;

/// Compiles session events into conversational context.
pub struct HistoryCompiler {
    session_store: Arc<dyn SessionStore>,
}

impl HistoryCompiler {
    /// Creates a history compiler backed by the shared session store.
    pub fn new(session_store: Arc<dyn SessionStore>) -> Self {
        Self { session_store }
    }

    /// Converts event records into context messages subject to the available budget.
    pub fn compile_messages(
        &self,
        events: &[EventRecord],
        remaining_budget: usize,
    ) -> Result<(Vec<ContextMessage>, usize)> {
        let checkpoint = events.iter().rev().find_map(|record| match &record.event {
            Event::Checkpoint { summary, .. } => Some((record.sequence_num, summary.clone())),
            _ => None,
        });

        let mut messages = Vec::new();
        let mut tokens_used = 0;
        let from_seq = checkpoint
            .as_ref()
            .map(|(sequence_num, _)| sequence_num + 1);

        if let Some((_, summary)) = &checkpoint {
            let checkpoint_message = ContextMessage::system(format!(
                "<session_checkpoint>\n{summary}\n</session_checkpoint>"
            ));
            tokens_used += estimate_tokens(&checkpoint_message.content);
            messages.push(checkpoint_message);
        }

        let turn_messages = events
            .iter()
            .filter(|record| {
                from_seq
                    .map(|sequence_num| record.sequence_num >= sequence_num)
                    .unwrap_or(true)
            })
            .filter_map(event_to_context_message)
            .collect::<Result<Vec<_>>>()?;

        let split_index = turn_messages.len().saturating_sub(RECENT_MESSAGE_LIMIT);
        let (older_messages, recent_messages) = turn_messages.split_at(split_index);

        for message in recent_messages {
            tokens_used += estimate_tokens(&message.content);
            messages.push(message.clone());
        }

        for message in older_messages.iter().rev() {
            let message_tokens = estimate_tokens(&message.content);
            if tokens_used + message_tokens > remaining_budget {
                break;
            }

            tokens_used += message_tokens;
            let insert_at = messages.len().saturating_sub(recent_messages.len());
            messages.insert(insert_at, message.clone());
        }

        for record in events {
            if let Event::Error { message, .. } = &record.event {
                let tagged = format!("<previous_error>{message}</previous_error>");
                if !messages.iter().any(|entry| entry.content == tagged) {
                    messages.insert(0, ContextMessage::system(tagged.clone()));
                    tokens_used += estimate_tokens(&tagged);
                }
            }
        }

        Ok((messages, tokens_used))
    }
}

#[async_trait]
impl ContextProcessor for HistoryCompiler {
    fn name(&self) -> &str {
        "history"
    }

    fn stage(&self) -> u8 {
        6
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let remaining_budget = ctx.token_budget.saturating_sub(ctx.token_count);
        let events = self
            .session_store
            .get_events(ctx.session_id.clone(), EventRange::all())
            .await?;
        let (messages, tokens_added) = self.compile_messages(&events, remaining_budget)?;
        let items_included = messages
            .iter()
            .map(|message| format!("{:?}", message.role))
            .collect::<Vec<_>>();

        ctx.extend_messages(messages);

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

fn event_to_context_message(record: &EventRecord) -> Option<Result<ContextMessage>> {
    match &record.event {
        Event::UserMessage { text, .. } => Some(Ok(ContextMessage::user(text.clone()))),
        Event::QueuedMessage { text, .. } => Some(Ok(ContextMessage::user(text.clone()))),
        Event::BrainResponse { text, .. } => Some(Ok(ContextMessage::assistant(text.clone()))),
        Event::ToolCall {
            tool_name, input, ..
        } => Some(
            serde_json::to_string(input)
                .map(|serialized| {
                    ContextMessage::assistant(format!(
                        "<tool_call name=\"{tool_name}\">{serialized}</tool_call>"
                    ))
                })
                .map_err(Into::into),
        ),
        Event::ToolResult {
            output,
            success,
            tool_id,
            ..
        } => Some(Ok(ContextMessage::tool(format!(
            "<tool_result id=\"{tool_id}\" success=\"{success}\">\n{output}\n</tool_result>"
        )))),
        Event::ToolError { error, tool_id, .. } => Some(Ok(ContextMessage::tool(format!(
            "<tool_error id=\"{tool_id}\">{error}</tool_error>"
        )))),
        Event::Warning { message } => Some(Ok(ContextMessage::system(format!(
            "<warning>{message}</warning>"
        )))),
        Event::MemoryRead { path, scope } => Some(Ok(ContextMessage::system(format!(
            "<memory_read scope=\"{scope}\">{path}</memory_read>"
        )))),
        Event::MemoryWrite { path, summary, .. } => Some(Ok(ContextMessage::system(format!(
            "<memory_write path=\"{path}\">{summary}</memory_write>"
        )))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::{
        BrainId, EventFilter, EventRecord, Platform, SequenceNum, SessionFilter, SessionId,
        SessionMeta, SessionStatus, SessionStore, SessionSummary, TokenPricing, ToolCallFormat,
        UserId, WorkspaceId,
    };
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Clone)]
    struct MockSessionStore {
        session: Arc<Mutex<SessionMeta>>,
        events: Arc<Mutex<Vec<EventRecord>>>,
    }

    impl MockSessionStore {
        fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
            Self {
                session: Arc::new(Mutex::new(session)),
                events: Arc::new(Mutex::new(events)),
            }
        }
    }

    #[async_trait]
    impl SessionStore for MockSessionStore {
        async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
            let id = meta.id.clone();
            *self.session.lock().await = meta;
            Ok(id)
        }

        async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum> {
            let mut events = self.events.lock().await;
            let sequence_num = events.len() as SequenceNum;
            events.push(EventRecord {
                id: uuid::Uuid::new_v4(),
                session_id,
                sequence_num,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            });
            Ok(sequence_num)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            Ok(self.events.lock().await.clone())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(self.session.lock().await.clone())
        }

        async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
            self.session.lock().await.status = status;
            Ok(())
        }

        async fn search_events(
            &self,
            _query: &str,
            _filter: EventFilter,
        ) -> Result<Vec<EventRecord>> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }
    }

    fn capabilities() -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: "claude-sonnet-4-6".to_string(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
        }
    }

    fn event_record(session_id: &SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: uuid::Uuid::new_v4(),
            session_id: session_id.clone(),
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: Option::<BrainId>::None,
            hand_id: None,
            token_count: None,
        }
    }

    #[test]
    fn history_compiler_formats_user_and_assistant_turns() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        };
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "Hello".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::BrainResponse {
                    text: "Hi there".to_string(),
                    model: "claude-sonnet-4-6".to_string(),
                    input_tokens: 10,
                    output_tokens: 4,
                    cost_cents: 1,
                    duration_ms: 100,
                },
            ),
        ];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, tokens_added) = compiler.compile_messages(&events, 1_000).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, moa_core::MessageRole::User);
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[1].role, moa_core::MessageRole::Assistant);
        assert_eq!(messages[1].content, "Hi there");
        assert!(tokens_added > 0);
    }

    #[tokio::test]
    async fn history_processor_loads_events_directly_from_session_store() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        };
        let events = vec![event_record(
            &session.id,
            0,
            Event::UserMessage {
                text: "Hello".to_string(),
                attachments: Vec::new(),
            },
        )];
        let mut ctx = WorkingContext::new(&session, capabilities());
        let compiler =
            HistoryCompiler::new(Arc::new(MockSessionStore::new(session.clone(), events)));

        let output = compiler.process(&mut ctx).await.unwrap();

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].content, "Hello");
        assert!(output.tokens_added > 0);
    }
}
