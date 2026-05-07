//! Test support for history compiler unit tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    BrainId, CONTEXT_SNAPSHOT_FORMAT_VERSION, CompactionConfig, CompletionContent,
    CompletionRequest, CompletionResponse, CompletionStream, ContextSnapshot,
    ContextSnapshotConfig, Event, EventFilter, EventRange, EventRecord, LLMProvider, ModelId,
    PendingSignal, PendingSignalId, Platform, Result, SequenceNum, SessionFilter, SessionId,
    SessionMeta, SessionStatus, SessionStore, SessionSummary, StopReason, TokenPricing, TokenUsage,
    ToolCallFormat, ToolCallId, ToolOutput, ToolOutputConfig, UserId, WorkspaceId,
};
use tokio::sync::Mutex;

use super::{CompiledHistory, HistoryCompiler};

pub(crate) mod prelude {
    pub(crate) use std::sync::Arc;
    pub(crate) use std::time::Duration;

    pub(crate) use chrono::Utc;
    pub(crate) use moa_core::{
        CONTEXT_SNAPSHOT_FORMAT_VERSION, CompactionConfig, ContextMessage, ContextProcessor,
        ContextSnapshot, Event, EventRange, EventRecord, FileReadDedupState, ModelId, SessionMeta,
        SessionStore, ToolCallId, ToolContent, ToolOutput, ToolOutputConfig, WorkingContext,
    };
    pub(crate) use proptest::prelude::*;
    pub(crate) use serde_json::json;

    pub(crate) use super::{
        MockLlmProvider, MockSessionStore, capabilities, compiled_snapshot,
        compiler_with_recent_turns, event_record, file_read_tool_call, file_read_tool_result,
        session,
    };
    pub(crate) use crate::pipeline::history::{FILE_READ_DEDUP_PLACEHOLDER, HistoryCompiler};
}

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}

#[derive(Clone)]
pub(crate) struct MockSessionStore {
    session: Arc<Mutex<SessionMeta>>,
    events: Arc<Mutex<Vec<EventRecord>>>,
    snapshot: Arc<Mutex<Option<ContextSnapshot>>>,
}

impl MockSessionStore {
    pub(crate) fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            events: Arc::new(Mutex::new(events)),
            snapshot: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl SessionStore for MockSessionStore {
    async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
        let id = meta.id;
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

    async fn put_snapshot(&self, _session_id: SessionId, snapshot: ContextSnapshot) -> Result<()> {
        *self.snapshot.lock().await = Some(snapshot);
        Ok(())
    }

    async fn get_snapshot(&self, _session_id: SessionId) -> Result<Option<ContextSnapshot>> {
        Ok(self.snapshot.lock().await.clone())
    }

    async fn delete_snapshot(&self, _session_id: SessionId) -> Result<()> {
        *self.snapshot.lock().await = None;
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

    async fn search_events(&self, _query: &str, _filter: EventFilter) -> Result<Vec<EventRecord>> {
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
pub(crate) struct MockLlmProvider;

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
            model: ModelId::new("claude-sonnet-4-6"),
            usage: token_usage(120, 40),
            duration_ms: 25,
            thought_signature: None,
        }))
    }
}

pub(crate) fn capabilities() -> moa_core::ModelCapabilities {
    moa_core::ModelCapabilities {
        model_id: ModelId::new("claude-sonnet-4-6"),
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

pub(crate) fn event_record(session_id: &SessionId, sequence_num: u64, event: Event) -> EventRecord {
    EventRecord {
        id: uuid::Uuid::now_v7(),
        session_id: *session_id,
        sequence_num,
        event_type: event.event_type(),
        event,
        timestamp: Utc::now(),
        brain_id: Option::<BrainId>::None,
        hand_id: None,
        token_count: None,
    }
}

pub(crate) fn session() -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        platform: Platform::Cli,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

pub(crate) fn compiler_with_recent_turns(
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
        snapshot_config: ContextSnapshotConfig::default(),
    }
}

pub(crate) fn file_read_tool_call(
    session_id: &SessionId,
    sequence_num: u64,
    tool_id: ToolCallId,
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

pub(crate) fn file_read_tool_result(
    session_id: &SessionId,
    sequence_num: u64,
    tool_id: ToolCallId,
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
            original_output_tokens: None,
            success: true,
            duration_ms: 5,
        },
    )
}

pub(crate) fn compiled_snapshot(
    session: &SessionMeta,
    compiled: &CompiledHistory,
) -> Option<ContextSnapshot> {
    compiled.snapshot.as_ref().map(|snapshot| ContextSnapshot {
        format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
        session_id: session.id,
        last_sequence_num: snapshot.last_sequence_num,
        created_at: Utc::now(),
        messages: snapshot.messages.clone(),
        file_read_dedup_state: snapshot.file_read_dedup_state.clone(),
        token_count: snapshot.token_count,
        cache_controls: Vec::new(),
        stage_inputs_hash: 1,
    })
}
