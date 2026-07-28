//! Test support for history compiler unit tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_config::CompactionConfig;
use moa_config::ContextSnapshotConfig;
use moa_config::ToolOutputConfig;
use moa_core::{
    error::Result, events::Event, traits::LLMProvider, traits::SessionStore,
    types::channel::Channel, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::CompletionStream, types::completion::StopReason,
    types::completion::TokenUsage, types::events_stream::EventFilter,
    types::events_stream::EventRange, types::events_stream::EventRecord,
    types::events_stream::SequenceNum, types::identifiers::BrainId, types::identifiers::ModelId,
    types::identifiers::SessionId, types::identifiers::TenantId, types::identifiers::ToolCallId,
    types::model::TokenPricing, types::model::ToolCallFormat, types::session::SessionFilter,
    types::session::SessionMeta, types::session::SessionStatus, types::session::SessionSummary,
    types::snapshot::CONTEXT_SNAPSHOT_FORMAT_VERSION, types::snapshot::ContextSnapshot,
    types::tools::ToolOutput,
};
use tokio::sync::Mutex;

use super::{CompiledHistory, HistoryCompiler};

pub(crate) mod prelude {
    pub(crate) use std::sync::Arc;
    pub(crate) use std::time::Duration;

    pub(crate) use chrono::Utc;
    pub(crate) use moa_config::CompactionConfig;
    pub(crate) use moa_config::ToolOutputConfig;
    pub(crate) use moa_core::{
        events::Event, traits::ContextProcessor, traits::SessionStore,
        types::context::ContextMessage, types::context::WorkingContext,
        types::events_stream::EventRange, types::events_stream::EventRecord,
        types::identifiers::ModelId, types::identifiers::ToolCallId, types::session::SessionMeta,
        types::snapshot::CONTEXT_SNAPSHOT_FORMAT_VERSION, types::snapshot::ContextSnapshot,
        types::snapshot::FileReadDedupState, types::tools::ToolContent, types::tools::ToolOutput,
    };
    pub(crate) use proptest::prelude::*;
    pub(crate) use serde_json::json;

    pub(crate) use super::{
        MockLlmProvider, MockSessionStore, capabilities, compiled_snapshot,
        compiler_with_recent_turns, event_record, file_read_tool_call, file_read_tool_result,
        session,
    };
    pub(crate) use crate::pipeline::history::{
        FILE_READ_DEDUP_PLACEHOLDER, FILE_READ_UNCHANGED_PLACEHOLDER, HistoryCompiler,
        SUPERSEDED_TOOL_RESULT_PLACEHOLDER,
    };
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
    snapshot_writes: Arc<Mutex<usize>>,
    snapshot_deletes: Arc<Mutex<usize>>,
}

impl MockSessionStore {
    pub(crate) fn new(mut session: SessionMeta, events: Vec<EventRecord>) -> Self {
        // Mirror the app-maintained session aggregates so the compaction
        // watermark gate sees a faithful event count and checkpoint sequence.
        session.event_count = events.len();
        session.last_checkpoint_seq = events
            .iter()
            .rev()
            .find(|record| matches!(record.event, Event::Checkpoint { .. }))
            .map(|record| record.sequence_num);
        Self {
            session: Arc::new(Mutex::new(session)),
            events: Arc::new(Mutex::new(events)),
            snapshot: Arc::new(Mutex::new(None)),
            snapshot_writes: Arc::new(Mutex::new(0)),
            snapshot_deletes: Arc::new(Mutex::new(0)),
        }
    }

    pub(crate) async fn snapshot_write_count(&self) -> usize {
        *self.snapshot_writes.lock().await
    }

    pub(crate) async fn snapshot_delete_count(&self) -> usize {
        *self.snapshot_deletes.lock().await
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
        let is_checkpoint = matches!(event, Event::Checkpoint { .. });
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
        let event_count = events.len();
        drop(events);
        // Keep the app-maintained aggregates consistent with the log.
        let mut session = self.session.lock().await;
        session.event_count = event_count;
        if is_checkpoint {
            session.last_checkpoint_seq = Some(sequence_num);
        }
        Ok(sequence_num)
    }

    async fn get_events(
        &self,
        _session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        let mut events = self
            .events
            .lock()
            .await
            .iter()
            .filter(|record| {
                range
                    .from_seq
                    .is_none_or(|from_seq| record.sequence_num >= from_seq)
                    && range
                        .to_seq
                        .is_none_or(|to_seq| record.sequence_num <= to_seq)
                    && range
                        .event_types
                        .as_ref()
                        .is_none_or(|event_types| event_types.contains(&record.event_type))
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(limit) = range.limit
            && events.len() > limit
        {
            events = events.split_off(events.len() - limit);
        }
        Ok(events)
    }

    async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
        Ok(self.session.lock().await.clone())
    }

    async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
        self.session.lock().await.status = status;
        Ok(())
    }

    async fn put_snapshot(&self, _session_id: SessionId, snapshot: ContextSnapshot) -> Result<()> {
        *self.snapshot_writes.lock().await += 1;
        *self.snapshot.lock().await = Some(snapshot);
        Ok(())
    }

    async fn get_snapshot(&self, _session_id: SessionId) -> Result<Option<ContextSnapshot>> {
        Ok(self.snapshot.lock().await.clone())
    }

    async fn delete_snapshot(&self, _session_id: SessionId) -> Result<()> {
        *self.snapshot_deletes.lock().await += 1;
        *self.snapshot.lock().await = None;
        Ok(())
    }

    async fn search_events(&self, _query: &str, _filter: EventFilter) -> Result<Vec<EventRecord>> {
        Ok(Vec::new())
    }

    async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }

    async fn tenant_cost_since(&self, _tenant_id: &TenantId, _since: DateTime<Utc>) -> Result<u32> {
        Ok(0)
    }

    async fn delete_empty_session(&self, _session_id: SessionId) -> Result<()> {
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

    fn capabilities(&self) -> moa_core::types::model::ModelCapabilities {
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

pub(crate) fn capabilities() -> moa_core::types::model::ModelCapabilities {
    moa_core::types::model::ModelCapabilities {
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
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
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
        tenant_id: TenantId::new(),
        channel: Channel::Chat,
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
            assessment: moa_core::types::security::ToolOutputAssessment::safe(),
            capability: moa_core::types::security::ToolCapabilityId::builtin("file_read"),
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
        stage_inputs_hash: 1,
    })
}
