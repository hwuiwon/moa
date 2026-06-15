//! Test fixtures for compactor unit tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    BrainId, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    ContextSnapshot, Event, EventFilter, EventRange, EventRecord, LLMProvider, ModelCapabilities,
    ModelId, Platform, Result, SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore,
    SessionSummary, StopReason, TokenPricing, TokenUsage, ToolCallFormat, WorkspaceId,
};
use tokio::sync::Mutex;

#[derive(Clone)]
pub(super) struct MockSessionStore {
    session: Arc<Mutex<SessionMeta>>,
    events: Arc<Mutex<Vec<EventRecord>>>,
}

impl MockSessionStore {
    pub(super) fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            events: Arc::new(Mutex::new(events)),
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

    async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
        Ok(self.session.lock().await.clone())
    }

    async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }

    async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<u64> {
        let mut events = self.events.lock().await;
        let sequence_num = events.len() as u64 + 1;
        let record = EventRecord {
            id: uuid::Uuid::now_v7(),
            session_id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: Option::<BrainId>::None,
            hand_id: None,
            token_count: None,
        };
        events.push(record);
        Ok(sequence_num)
    }

    async fn get_events(
        &self,
        _session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        let events = self.events.lock().await.clone();
        Ok(events
            .into_iter()
            .filter(|record| {
                range
                    .from_seq
                    .map(|from_seq| record.sequence_num >= from_seq)
                    .unwrap_or(true)
                    && range
                        .to_seq
                        .map(|to_seq| record.sequence_num <= to_seq)
                        .unwrap_or(true)
            })
            .collect())
    }

    async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
        self.session.lock().await.status = status;
        Ok(())
    }

    async fn put_snapshot(&self, _session_id: SessionId, _snapshot: ContextSnapshot) -> Result<()> {
        Ok(())
    }

    async fn get_snapshot(&self, _session_id: SessionId) -> Result<Option<ContextSnapshot>> {
        Ok(None)
    }

    async fn delete_snapshot(&self, _session_id: SessionId) -> Result<()> {
        Ok(())
    }

    async fn search_events(&self, _query: &str, _filter: EventFilter) -> Result<Vec<EventRecord>> {
        Ok(Vec::new())
    }

    async fn workspace_cost_since(
        &self,
        _workspace_id: &WorkspaceId,
        _since: DateTime<Utc>,
    ) -> Result<u32> {
        Ok(0)
    }

    async fn delete_empty_session(&self, _session_id: SessionId) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct MockLlmProvider;

#[async_trait]
impl LLMProvider for MockLlmProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> ModelCapabilities {
        capabilities()
    }

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
        Ok(CompletionStream::from_response(CompletionResponse {
            text: "## Goal\n- compact the older turns\n".to_string(),
            content: vec![CompletionContent::Text(
                "## Goal\n- compact the older turns\n".to_string(),
            )],
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("claude-sonnet-4-6"),
            usage: TokenUsage {
                input_tokens_uncached: 120,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 40,
            },
            duration_ms: 25,
            thought_signature: None,
        }))
    }
}

pub(super) fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("claude-sonnet-4-6"),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl: Some(Duration::from_secs(300)),
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

pub(super) fn session() -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("workspace"),
        user_id: moa_core::UserId::new("user"),
        platform: Platform::Api,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

pub(super) fn event_record(session_id: &SessionId, sequence_num: u64, event: Event) -> EventRecord {
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
