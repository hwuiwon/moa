//! Stage 6: compiles session history into context messages.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    CompactionConfig, ContextMessage, ContextProcessor, Event, EventRange, EventRecord,
    LLMProvider, ProcessorOutput, Result, SessionStore, ToolContent, ToolOutput, ToolOutputConfig,
    WorkingContext, truncate_head_tail,
};
use moa_security::wrap_untrusted_tool_output;
use serde_json::json;

use crate::compaction::{
    latest_checkpoint_state, maybe_compact_events, non_checkpoint_events, recent_turn_boundary,
    unsummarized_events,
};

use super::estimate_tokens;

const FILE_READ_DEDUP_PLACEHOLDER: &str = "[file previously read — see latest version below]";

/// Compiles session events into conversational context.
pub struct HistoryCompiler {
    session_store: Arc<dyn SessionStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
    compaction: CompactionConfig,
    tool_output: ToolOutputConfig,
}

impl HistoryCompiler {
    /// Creates a history compiler without automatic checkpoint generation.
    pub fn new(session_store: Arc<dyn SessionStore>) -> Self {
        Self {
            session_store,
            llm_provider: None,
            compaction: CompactionConfig::default(),
            tool_output: ToolOutputConfig::default(),
        }
    }

    /// Overrides the compaction and replay-window settings used during history compilation.
    pub fn with_compaction_config(mut self, compaction: CompactionConfig) -> Self {
        self.compaction = compaction;
        self
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
            tool_output: ToolOutputConfig::default(),
        }
    }

    /// Overrides the tool-output truncation settings used during history replay.
    pub fn with_tool_output_config(mut self, tool_output: ToolOutputConfig) -> Self {
        self.tool_output = tool_output;
        self
    }

    /// Converts event records into context messages subject to the available budget.
    pub fn compile_messages(
        &self,
        events: &[EventRecord],
        remaining_budget: usize,
    ) -> Result<(Vec<ContextMessage>, usize)> {
        let compiled = self.compile_messages_with_stats(events, remaining_budget)?;
        Ok((compiled.messages, compiled.tokens_used))
    }

    fn compile_messages_with_stats(
        &self,
        events: &[EventRecord],
        remaining_budget: usize,
    ) -> Result<CompiledHistory> {
        let checkpoint = latest_checkpoint_state(events);
        let all_non_checkpoint = non_checkpoint_events(events);
        let visible_events = unsummarized_events(events);
        let recent_start =
            recent_turn_boundary(&visible_events, self.compaction.recent_turns_verbatim);
        let (older_events, recent_events) = visible_events.split_at(recent_start);
        let file_read_paths = build_full_file_read_path_map(&visible_events);
        let latest_file_reads = latest_full_file_read_results(&visible_events, &file_read_paths);

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

        let recent_messages = compile_records(recent_events, &self.tool_output, &file_read_paths)?;
        let mut older_messages =
            compile_records(older_events, &self.tool_output, &file_read_paths)?;
        let deduplication = deduplicate_file_reads(&mut older_messages, &latest_file_reads);

        for compiled in &recent_messages {
            tokens_used += estimate_tokens(&compiled.message.content);
            messages.push(compiled.message.clone());
        }

        for compiled in older_messages.iter().rev() {
            let message_tokens = estimate_tokens(&compiled.message.content);
            if tokens_used + message_tokens > remaining_budget {
                break;
            }

            tokens_used += message_tokens;
            let insert_at = messages.len().saturating_sub(recent_messages.len());
            messages.insert(insert_at, compiled.message.clone());
        }

        Ok(CompiledHistory {
            messages,
            tokens_used,
            deduplication,
        })
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
        let compiled = self.compile_messages_with_stats(&events, remaining_budget)?;
        if compiled.deduplication.deduplicated_count > 0 {
            tracing::info!(
                deduplicated = compiled.deduplication.deduplicated_count,
                tokens_saved = compiled.deduplication.tokens_saved,
                "deduplicated file read results in history compilation"
            );
        }
        let messages = compiled.messages;
        let tokens_added = compiled.tokens_used;
        let items_included = messages
            .iter()
            .map(|message| format!("{:?}", message.role))
            .collect::<Vec<_>>();

        ctx.extend_messages(messages);

        let mut metadata = HashMap::new();
        metadata.insert(
            "file_reads_deduplicated".to_string(),
            json!(compiled.deduplication.deduplicated_count),
        );
        metadata.insert(
            "tokens_saved_by_dedup".to_string(),
            json!(compiled.deduplication.tokens_saved),
        );

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            metadata,
            ..ProcessorOutput::default()
        })
    }
}

fn compile_records(
    records: &[&EventRecord],
    tool_output: &ToolOutputConfig,
    file_read_paths: &HashMap<uuid::Uuid, String>,
) -> Result<Vec<CompiledRecordMessage>> {
    records
        .iter()
        .filter_map(|record| event_to_context_message(record, tool_output, file_read_paths))
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

fn event_to_context_message(
    record: &EventRecord,
    tool_output: &ToolOutputConfig,
    file_read_paths: &HashMap<uuid::Uuid, String>,
) -> Option<Result<CompiledRecordMessage>> {
    match &record.event {
        Event::UserMessage { text, .. } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::user(text.clone()),
        ))),
        Event::QueuedMessage { text, .. } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::user(text.clone()),
        ))),
        Event::BrainResponse {
            text,
            thought_signature,
            ..
        } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::assistant_with_thought_signature(
                text.clone(),
                thought_signature.clone(),
            ),
        ))),
        Event::ToolCall {
            tool_id,
            provider_tool_use_id,
            provider_thought_signature,
            tool_name,
            input,
            ..
        } => Some(
            serde_json::to_string(input)
                .map(|serialized| {
                    CompiledRecordMessage::plain(
                        ContextMessage::assistant_tool_call_with_thought_signature(
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
                            provider_thought_signature.clone(),
                        ),
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
        } => Some(Ok(tool_result_context_message(
            provider_tool_use_id
                .clone()
                .unwrap_or_else(|| tool_id.to_string()),
            *tool_id,
            *success,
            output,
            tool_output,
            file_read_paths.get(tool_id).cloned(),
        ))),
        Event::ToolError {
            error,
            tool_id,
            provider_tool_use_id,
            ..
        } => Some(Ok(CompiledRecordMessage::plain(
            match provider_tool_use_id.as_ref() {
                Some(call_id) => {
                    let replayable_error = truncate_tool_result_text(error, tool_output);
                    ContextMessage::tool_result(
                        call_id.clone(),
                        format!("<tool_error id=\"{tool_id}\">{replayable_error}</tool_error>"),
                        Some(vec![ToolContent::Text {
                            text: replayable_error,
                        }]),
                    )
                }
                None => ContextMessage::tool(format!(
                    "<tool_error id=\"{tool_id}\">{error}</tool_error>"
                )),
            },
        ))),
        Event::Warning { message } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::system(format!("<warning>{message}</warning>")),
        ))),
        Event::MemoryRead { path, scope } => {
            Some(Ok(CompiledRecordMessage::plain(ContextMessage::system(
                format!("<memory_read scope=\"{scope}\">{path}</memory_read>"),
            ))))
        }
        Event::MemoryWrite { path, summary, .. } => {
            Some(Ok(CompiledRecordMessage::plain(ContextMessage::system(
                format!("<memory_write path=\"{path}\">{summary}</memory_write>"),
            ))))
        }
        Event::MemoryIngest {
            source_name,
            source_path,
            ..
        } => Some(Ok(CompiledRecordMessage::plain(ContextMessage::system(
            format!(
                "<memory_ingest source_name=\"{source_name}\" source_path=\"{source_path}\" />"
            ),
        )))),
        _ => None,
    }
}

fn tool_result_context_message(
    tool_use_id: String,
    tool_id: uuid::Uuid,
    success: bool,
    output: &ToolOutput,
    tool_output: &ToolOutputConfig,
    file_read_path: Option<String>,
) -> CompiledRecordMessage {
    let replayable_text = truncate_tool_result_text(&output.to_text(), tool_output);
    CompiledRecordMessage {
        message: ContextMessage::tool_result(
            tool_use_id.clone(),
            format!(
                "<tool_result id=\"{tool_id}\" success=\"{success}\">\n{}\n</tool_result>",
                wrap_untrusted_tool_output(&replayable_text)
            ),
            replayable_tool_content_blocks(output, &replayable_text, tool_output),
        ),
        tool_result: file_read_path.as_ref().map(|path| ToolResultReplayMeta {
            tool_use_id,
            tool_id,
            success,
            file_read_path: path.clone(),
        }),
    }
}

fn build_full_file_read_path_map(events: &[&EventRecord]) -> HashMap<uuid::Uuid, String> {
    let mut file_reads = HashMap::new();

    for record in events {
        let Event::ToolCall {
            tool_id,
            tool_name,
            input,
            ..
        } = &record.event
        else {
            continue;
        };

        if tool_name != "file_read" {
            continue;
        }

        let Some(path) = input.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };

        if input.get("start_line").is_some() || input.get("end_line").is_some() {
            continue;
        }

        file_reads.insert(*tool_id, path.to_string());
    }

    file_reads
}

fn latest_full_file_read_results(
    events: &[&EventRecord],
    file_read_paths: &HashMap<uuid::Uuid, String>,
) -> HashMap<String, uuid::Uuid> {
    let mut latest_results = HashMap::new();

    for record in events {
        let Event::ToolResult { tool_id, .. } = &record.event else {
            continue;
        };

        let Some(path) = file_read_paths.get(tool_id) else {
            continue;
        };

        latest_results.insert(path.clone(), *tool_id);
    }

    latest_results
}

fn deduplicate_file_reads(
    messages: &mut [CompiledRecordMessage],
    latest_file_reads: &HashMap<String, uuid::Uuid>,
) -> DeduplicationStats {
    let mut stats = DeduplicationStats::default();

    for compiled in messages {
        let Some(tool_result) = compiled.tool_result.as_ref() else {
            continue;
        };
        let Some(latest_tool_id) = latest_file_reads.get(&tool_result.file_read_path) else {
            continue;
        };
        if tool_result.tool_id == *latest_tool_id {
            continue;
        }

        let previous_tokens = estimate_tokens(&compiled.message.content);
        compiled.message = placeholder_tool_result_message(tool_result);
        let placeholder_tokens = estimate_tokens(&compiled.message.content);
        stats.deduplicated_count += 1;
        stats.tokens_saved += previous_tokens.saturating_sub(placeholder_tokens);
    }

    stats
}

fn placeholder_tool_result_message(tool_result: &ToolResultReplayMeta) -> ContextMessage {
    let placeholder = FILE_READ_DEDUP_PLACEHOLDER.to_string();

    ContextMessage::tool_result(
        tool_result.tool_use_id.clone(),
        format!(
            "<tool_result id=\"{}\" success=\"{}\">\n{}\n</tool_result>",
            tool_result.tool_id,
            tool_result.success,
            wrap_untrusted_tool_output(&placeholder)
        ),
        Some(vec![ToolContent::Text { text: placeholder }]),
    )
}

fn replayable_tool_content_blocks(
    output: &ToolOutput,
    replayable_text: &str,
    tool_output: &ToolOutputConfig,
) -> Option<Vec<ToolContent>> {
    let total_chars = output
        .content
        .iter()
        .map(tool_content_char_len)
        .sum::<usize>();

    if total_chars <= tool_output.max_replay_chars {
        return Some(output.content.clone());
    }

    Some(vec![ToolContent::Text {
        text: replayable_text.to_string(),
    }])
}

fn tool_content_char_len(content: &ToolContent) -> usize {
    match content {
        ToolContent::Text { text } => text.chars().count(),
        ToolContent::Json { data } => data.to_string().chars().count(),
    }
}

fn truncate_tool_result_text(text: &str, tool_output: &ToolOutputConfig) -> String {
    truncate_head_tail(text, tool_output.max_replay_chars, tool_output.head_ratio).0
}

struct CompiledHistory {
    messages: Vec<ContextMessage>,
    tokens_used: usize,
    deduplication: DeduplicationStats,
}

#[derive(Debug, Clone)]
struct CompiledRecordMessage {
    message: ContextMessage,
    tool_result: Option<ToolResultReplayMeta>,
}

impl CompiledRecordMessage {
    fn plain(message: ContextMessage) -> Self {
        Self {
            message,
            tool_result: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ToolResultReplayMeta {
    tool_use_id: String,
    tool_id: uuid::Uuid,
    success: bool,
    file_read_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeduplicationStats {
    deduplicated_count: usize,
    tokens_saved: usize,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        BrainId, CompactionConfig, CompletionContent, CompletionRequest, CompletionResponse,
        CompletionStream, EventFilter, EventRecord, PendingSignal, PendingSignalId, Platform,
        SequenceNum, SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore,
        SessionSummary, StopReason, TokenPricing, TokenUsage, ToolCallFormat, ToolOutputConfig,
        UserId, WorkspaceId,
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::*;

    fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens_uncached: input_tokens,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        }
    }

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
                id: uuid::Uuid::now_v7(),
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

        async fn workspace_cost_since(
            &self,
            _workspace_id: &WorkspaceId,
            _since: DateTime<Utc>,
        ) -> Result<u32> {
            Ok(0)
        }

        async fn delete_session(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
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
                usage: token_usage(120, 40),
                duration_ms: 25,
                thought_signature: None,
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
            id: uuid::Uuid::now_v7(),
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
            platform: Platform::Desktop,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        }
    }

    fn compiler_with_recent_turns(
        session: &SessionMeta,
        events: &[EventRecord],
        recent_turns_verbatim: usize,
    ) -> HistoryCompiler {
        HistoryCompiler {
            session_store: Arc::new(MockSessionStore::new(session.clone(), events.to_vec())),
            llm_provider: None,
            compaction: CompactionConfig {
                recent_turns_verbatim,
                ..CompactionConfig::default()
            },
            tool_output: ToolOutputConfig::default(),
        }
    }

    fn file_read_tool_call(
        session_id: &SessionId,
        sequence_num: u64,
        tool_id: uuid::Uuid,
        provider_tool_use_id: &str,
        input: serde_json::Value,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: Some(provider_tool_use_id.to_string()),
                provider_thought_signature: None,
                tool_name: "file_read".to_string(),
                input,
                hand_id: None,
            },
        )
    }

    fn file_read_tool_result(
        session_id: &SessionId,
        sequence_num: u64,
        tool_id: uuid::Uuid,
        provider_tool_use_id: &str,
        text: &str,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some(provider_tool_use_id.to_string()),
                output: ToolOutput::text(text, Duration::from_millis(5)),
                success: true,
                duration_ms: 5,
            },
        )
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
                    input_tokens_uncached: 10,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 4,
                    cost_cents: 1,
                    duration_ms: 100,
                    thought_signature: None,
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
        let tool_id = uuid::Uuid::now_v7();
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
    fn history_compiler_truncates_oversized_tool_results_for_replay() {
        let session = session();
        let tool_id = uuid::Uuid::now_v7();
        let giant = (1..=15_000)
            .map(|index| format!("src/lib.rs:{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_large".to_string()),
                output: ToolOutput {
                    content: vec![ToolContent::Text {
                        text: giant.clone(),
                    }],
                    is_error: false,
                    structured: None,
                    duration: Duration::from_millis(7),
                    truncated: false,
                },
                success: true,
                duration_ms: 7,
            },
        )];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, _) = compiler.compile_messages(&events, 1_000_000).unwrap();

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert!(message.content.contains("[... ~"));
        let blocks = message
            .content_blocks
            .as_ref()
            .expect("bounded content blocks");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ToolContent::Text { text } => {
                assert!(text.contains("src/lib.rs:1"));
                assert!(text.contains("src/lib.rs:15000"));
                assert!(text.contains("[... ~"));
                assert!(text.chars().count() <= ToolOutputConfig::default().max_replay_chars);
            }
            ToolContent::Json { .. } => panic!("oversized replay should collapse to a text block"),
        }
    }

    #[test]
    fn history_compiler_preserves_structured_tool_call_invocation() {
        let session = session();
        let tool_id = uuid::Uuid::now_v7();
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: Some("toolu_history_call".to_string()),
                provider_thought_signature: None,
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

    #[test]
    fn history_compiler_deduplicates_repeated_full_file_reads() {
        let session = session();
        let foo_first = uuid::Uuid::now_v7();
        let bar = uuid::Uuid::now_v7();
        let foo_second = uuid::Uuid::now_v7();
        let first_read = (1..=80)
            .map(|line| format!("fn first_version_{line}() {{}}\n"))
            .collect::<String>();
        let second_read = (1..=80)
            .map(|line| format!("fn latest_version_{line}() {{}}\n"))
            .collect::<String>();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "first read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                foo_first,
                "toolu_foo_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 2, foo_first, "toolu_foo_first", &first_read),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "bar read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                bar,
                "toolu_bar",
                json!({ "path": "src/bar.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                5,
                bar,
                "toolu_bar",
                "fn bar() {\n    keep_me();\n}",
            ),
            event_record(
                &session.id,
                6,
                Event::UserMessage {
                    text: "second foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                7,
                foo_second,
                "toolu_foo_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 8, foo_second, "toolu_foo_second", &second_read),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        let first_foo_result = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_first"))
            .expect("first foo result present");
        let second_foo_result = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_second"))
            .expect("second foo result present");
        let bar_result = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_bar"))
            .expect("bar result present");

        assert_eq!(
            first_foo_result.content_blocks,
            Some(vec![ToolContent::Text {
                text: FILE_READ_DEDUP_PLACEHOLDER.to_string(),
            }])
        );
        assert_eq!(
            first_foo_result.tool_use_id.as_deref(),
            Some("toolu_foo_first")
        );
        assert!(second_foo_result.content.contains("latest_version_80"));
        assert!(bar_result.content.contains("keep_me"));
        assert_eq!(compiled.deduplication.deduplicated_count, 1);
        assert!(compiled.deduplication.tokens_saved > 0);
    }

    #[test]
    fn history_compiler_does_not_deduplicate_recent_turn_file_reads() {
        let session = session();
        let foo_first = uuid::Uuid::now_v7();
        let foo_second = uuid::Uuid::now_v7();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "setup".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "first foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                2,
                foo_first,
                "toolu_foo_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                3,
                foo_first,
                "toolu_foo_first",
                "fn foo() {\n    first_recent();\n}",
            ),
            event_record(
                &session.id,
                4,
                Event::UserMessage {
                    text: "second foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                5,
                foo_second,
                "toolu_foo_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                6,
                foo_second,
                "toolu_foo_second",
                "fn foo() {\n    second_recent();\n}",
            ),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 2);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        assert_eq!(compiled.deduplication.deduplicated_count, 0);
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("first_recent"))
        );
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("second_recent"))
        );
        assert!(
            compiled
                .messages
                .iter()
                .all(|message| !message.content.contains(FILE_READ_DEDUP_PLACEHOLDER))
        );
    }

    #[test]
    fn history_compiler_does_not_deduplicate_partial_file_reads() {
        let session = session();
        let partial_one = uuid::Uuid::now_v7();
        let partial_two = uuid::Uuid::now_v7();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "first partial".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                partial_one,
                "toolu_partial_one",
                json!({ "path": "src/foo.rs", "start_line": 1, "end_line": 40 }),
            ),
            file_read_tool_result(
                &session.id,
                2,
                partial_one,
                "toolu_partial_one",
                "[showing lines 1-40 of 200 total in src/foo.rs]\n     1\tfn foo() {}",
            ),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second partial".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                partial_two,
                "toolu_partial_two",
                json!({ "path": "src/foo.rs", "start_line": 41, "end_line": 80 }),
            ),
            file_read_tool_result(
                &session.id,
                5,
                partial_two,
                "toolu_partial_two",
                "[showing lines 41-80 of 200 total in src/foo.rs]\n    41\tfn bar() {}",
            ),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        assert_eq!(compiled.deduplication.deduplicated_count, 0);
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("showing lines 1-40"))
        );
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("showing lines 41-80"))
        );
    }

    #[tokio::test]
    async fn history_processor_reports_file_read_deduplication_metadata() {
        let session = session();
        let foo_first = uuid::Uuid::now_v7();
        let foo_second = uuid::Uuid::now_v7();
        let first_read = (1..=80)
            .map(|line| format!("fn first_version_{line}() {{}}\n"))
            .collect::<String>();
        let second_read = (1..=80)
            .map(|line| format!("fn latest_version_{line}() {{}}\n"))
            .collect::<String>();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "first foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                foo_first,
                "toolu_foo_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 2, foo_first, "toolu_foo_first", &first_read),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                foo_second,
                "toolu_foo_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 5, foo_second, "toolu_foo_second", &second_read),
        ];
        let mut ctx = WorkingContext::new(&session, capabilities());
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let output = compiler
            .process(&mut ctx)
            .await
            .expect("history should process");

        assert_eq!(
            output.metadata.get("file_reads_deduplicated"),
            Some(&json!(1))
        );
        assert!(
            output
                .metadata
                .get("tokens_saved_by_dedup")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|value| value > 0)
        );
        assert!(
            ctx.messages
                .iter()
                .any(|message| message.content.contains(FILE_READ_DEDUP_PLACEHOLDER))
        );
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
