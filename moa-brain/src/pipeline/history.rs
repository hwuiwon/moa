//! Stage 6: compiles session history into context messages.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    CompactionConfig, ContextMessage, ContextProcessor, Event, EventRange, EventRecord,
    LLMProvider, ProcessorOutput, Result, SessionStore, WorkingContext,
};
use moa_security::wrap_untrusted_tool_output;

use crate::compaction::{
    latest_checkpoint_state, maybe_compact_events, non_checkpoint_events, recent_turn_boundary,
    unsummarized_events,
};

use super::estimate_tokens;

/// Compiles session events into conversational context.
pub struct HistoryCompiler {
    session_store: Arc<dyn SessionStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
    compaction: CompactionConfig,
}

impl HistoryCompiler {
    /// Creates a history compiler without automatic checkpoint generation.
    pub fn new(session_store: Arc<dyn SessionStore>) -> Self {
        Self {
            session_store,
            llm_provider: None,
            compaction: CompactionConfig::default(),
        }
    }

    /// Creates a history compiler that can emit reversible checkpoint summaries.
    pub fn with_compaction(
        session_store: Arc<dyn SessionStore>,
        llm_provider: Arc<dyn LLMProvider>,
        compaction: CompactionConfig,
    ) -> Self {
        Self {
            session_store,
            llm_provider: Some(llm_provider),
            compaction,
        }
    }

    /// Converts event records into context messages subject to the available budget.
    pub fn compile_messages(
        &self,
        events: &[EventRecord],
        remaining_budget: usize,
    ) -> Result<(Vec<ContextMessage>, usize)> {
        let checkpoint = latest_checkpoint_state(events);
        let all_non_checkpoint = non_checkpoint_events(events);
        let visible_events = unsummarized_events(events);
        let recent_start =
            recent_turn_boundary(&visible_events, self.compaction.recent_turns_verbatim);
        let (older_events, recent_events) = visible_events.split_at(recent_start);

        let mut messages = Vec::new();
        let mut tokens_used = 0usize;

        if self.compaction.preserve_errors {
            let summarized_end = checkpoint
                .as_ref()
                .map(|state| state.events_summarized.min(all_non_checkpoint.len()))
                .unwrap_or(0);
            for message in preserved_error_messages(&all_non_checkpoint[..summarized_end]) {
                tokens_used += estimate_tokens(&message.content);
                messages.push(message);
            }
        }

        if let Some(checkpoint) = checkpoint {
            let checkpoint_message = ContextMessage::system(format!(
                "<session_checkpoint summarized_events=\"{}\">\n{}\n</session_checkpoint>",
                checkpoint.events_summarized, checkpoint.summary
            ));
            tokens_used += estimate_tokens(&checkpoint_message.content);
            messages.push(checkpoint_message);
        }

        let recent_messages = compile_records(recent_events)?;
        let older_messages = compile_records(older_events)?;
        for message in &recent_messages {
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
        let mut events = self
            .session_store
            .get_events(ctx.session_id.clone(), EventRange::all())
            .await?;

        if let Some(llm_provider) = &self.llm_provider
            && maybe_compact_events(
                &self.compaction,
                &*self.session_store,
                &**llm_provider,
                ctx.session_id.clone(),
                ctx.token_budget,
                &events,
            )
            .await?
        {
            events = self
                .session_store
                .get_events(ctx.session_id.clone(), EventRange::all())
                .await?;
        }

        let remaining_budget = ctx.token_budget.saturating_sub(ctx.token_count);
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

fn compile_records(records: &[&EventRecord]) -> Result<Vec<ContextMessage>> {
    records
        .iter()
        .filter_map(|record| event_to_context_message(record))
        .collect::<Result<Vec<_>>>()
}

fn preserved_error_messages(events: &[&EventRecord]) -> Vec<ContextMessage> {
    let mut messages = Vec::new();
    for record in events {
        match &record.event {
            Event::Error { message, .. } => messages.push(ContextMessage::system(format!(
                "<previous_error>{message}</previous_error>"
            ))),
            Event::ToolError { error, tool_id, .. } => messages.push(ContextMessage::tool(
                format!("<tool_error id=\"{tool_id}\">{error}</tool_error>"),
            )),
            _ => {}
        }
    }
    messages
}

fn event_to_context_message(record: &EventRecord) -> Option<Result<ContextMessage>> {
    match &record.event {
        Event::UserMessage { text, .. } => Some(Ok(ContextMessage::user(text.clone()))),
        Event::QueuedMessage { text, .. } => Some(Ok(ContextMessage::user(text.clone()))),
        Event::BrainResponse { text, .. } => Some(Ok(ContextMessage::assistant(text.clone()))),
        Event::ToolCall {
            tool_id,
            provider_tool_use_id,
            tool_name,
            input,
            ..
        } => Some(
            serde_json::to_string(input)
                .map(|serialized| {
                    ContextMessage::assistant_tool_call(
                        moa_core::ToolInvocation {
                            id: Some(
                                provider_tool_use_id
                                    .clone()
                                    .unwrap_or_else(|| tool_id.to_string()),
                            ),
                            name: tool_name.clone(),
                            input: input.clone(),
                        },
                        format!("<tool_call name=\"{tool_name}\">{serialized}</tool_call>"),
                    )
                })
                .map_err(Into::into),
        ),
        Event::ToolResult {
            output,
            success,
            tool_id,
            provider_tool_use_id,
            ..
        } => Some(Ok(ContextMessage::tool_result(
            provider_tool_use_id
                .clone()
                .unwrap_or_else(|| tool_id.to_string()),
            format!(
                "<tool_result id=\"{tool_id}\" success=\"{success}\">\n{}\n</tool_result>",
                wrap_untrusted_tool_output(&output.to_text())
            ),
            Some(output.content.clone()),
        ))),
        Event::ToolError {
            error,
            tool_id,
            provider_tool_use_id,
            ..
        } => Some(Ok(match provider_tool_use_id.as_ref() {
            Some(call_id) => ContextMessage::tool_result(
                call_id.clone(),
                format!("<tool_error id=\"{tool_id}\">{error}</tool_error>"),
                Some(vec![moa_core::ToolContent::Text {
                    text: error.clone(),
                }]),
            ),
            None => {
                ContextMessage::tool(format!("<tool_error id=\"{tool_id}\">{error}</tool_error>"))
            }
        })),
        Event::Warning { message } => Some(Ok(ContextMessage::system(format!(
            "<warning>{message}</warning>"
        )))),
        Event::MemoryRead { path, scope } => Some(Ok(ContextMessage::system(format!(
            "<memory_read scope=\"{scope}\">{path}</memory_read>"
        )))),
        Event::MemoryWrite { path, summary, .. } => Some(Ok(ContextMessage::system(format!(
            "<memory_write path=\"{path}\">{summary}</memory_write>"
        )))),
        Event::MemoryIngest {
            source_name,
            source_path,
            ..
        } => Some(Ok(ContextMessage::system(format!(
            "<memory_ingest source_name=\"{source_name}\" source_path=\"{source_path}\" />"
        )))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::{
        BrainId, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
        EventFilter, EventRecord, PendingSignal, PendingSignalId, Platform, SequenceNum,
        SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore, SessionSummary,
        StopReason, TokenPricing, ToolCallFormat, UserId, WorkspaceId,
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

        async fn store_pending_signal(
            &self,
            _session_id: SessionId,
            signal: PendingSignal,
        ) -> Result<PendingSignalId> {
            Ok(signal.id)
        }

        async fn get_pending_signals(&self, _session_id: SessionId) -> Result<Vec<PendingSignal>> {
            Ok(Vec::new())
        }

        async fn resolve_pending_signal(&self, _signal_id: PendingSignalId) -> Result<()> {
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

    #[derive(Clone)]
    struct MockLlmProvider;

    #[async_trait]
    impl LLMProvider for MockLlmProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> moa_core::ModelCapabilities {
            capabilities()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "## Key Facts\n- compacted history\n\n## Decisions\n- keep the recent tail verbatim\n".to_string(),
                content: vec![CompletionContent::Text(
                    "## Key Facts\n- compacted history\n\n## Decisions\n- keep the recent tail verbatim\n"
                        .to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: "claude-sonnet-4-6".to_string(),
                input_tokens: 120,
                output_tokens: 40,
                cached_input_tokens: 0,
                duration_ms: 25,
            }))
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
            native_tools: Vec::new(),
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

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        }
    }

    #[test]
    fn history_compiler_formats_user_and_assistant_turns() {
        let session = session();
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

    #[test]
    fn history_compiler_preserves_structured_tool_result_blocks() {
        let session = session();
        let tool_id = uuid::Uuid::new_v4();
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_history".to_string()),
                output: moa_core::ToolOutput::json(
                    "1 result",
                    serde_json::json!({ "matches": ["notes/today.md"] }),
                    Duration::from_millis(7),
                ),
                success: true,
                duration_ms: 7,
            },
        )];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, _) = compiler.compile_messages(&events, 1_000).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_use_id.as_deref(), Some("toolu_history"));
        assert!(messages[0].content.contains("<tool_result"));
        assert_eq!(messages[0].content_blocks.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn history_compiler_preserves_structured_tool_call_invocation() {
        let session = session();
        let tool_id = uuid::Uuid::new_v4();
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: Some("toolu_history_call".to_string()),
                tool_name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "pwd" }),
                hand_id: None,
            },
        )];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, _) = compiler.compile_messages(&events, 1_000).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]
                .tool_invocation
                .as_ref()
                .and_then(|invocation| invocation.id.as_deref()),
            Some("toolu_history_call")
        );
        assert_eq!(
            messages[0]
                .tool_invocation
                .as_ref()
                .map(|invocation| invocation.name.as_str()),
            Some("bash")
        );
        assert!(messages[0].content.contains("<tool_call"));
    }

    #[tokio::test]
    async fn history_processor_loads_events_directly_from_session_store() {
        let session = session();
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

    #[tokio::test]
    async fn compaction_triggers_at_threshold_and_keeps_full_log() {
        let session = session();
        let mut events = Vec::new();
        for index in 0..7 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("event {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 4,
                recent_turns_verbatim: 2,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();
        let stored_events = store
            .get_events(session.id.clone(), EventRange::all())
            .await
            .unwrap();

        assert_eq!(stored_events.len(), 8);
        assert!(matches!(
            stored_events.last().map(|record| &record.event),
            Some(Event::Checkpoint { events_summarized, .. }) if *events_summarized == 5
        ));
    }

    #[tokio::test]
    async fn compacted_view_preserves_old_errors_and_respects_budget() {
        let session = session();
        let mut events = vec![event_record(
            &session.id,
            0,
            Event::Error {
                message: "deploy failed on port binding".to_string(),
                recoverable: true,
            },
        )];
        for index in 1..12 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("turn {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        events.push(event_record(
            &session.id,
            12,
            Event::Checkpoint {
                summary: "## Key Facts\n- earlier turns were compacted".to_string(),
                events_summarized: 8,
                token_count: 12,
                model: "claude-sonnet-4-6".to_string(),
                input_tokens: 60,
                output_tokens: 20,
                cost_cents: 1,
            },
        ));
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, tokens_used) = compiler.compile_messages(&events, 80).unwrap();
        let rendered = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("deploy failed on port binding"));
        assert!(rendered.contains("<session_checkpoint"));
        assert!(tokens_used <= 120);
    }

    #[tokio::test]
    async fn no_compaction_below_threshold() {
        let session = session();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "one".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "two".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 10,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();
        let stored_events = store
            .get_events(session.id.clone(), EventRange::all())
            .await
            .unwrap();

        assert_eq!(stored_events.len(), 2);
        assert!(
            !stored_events
                .iter()
                .any(|record| matches!(record.event, Event::Checkpoint { .. }))
        );
    }
}
