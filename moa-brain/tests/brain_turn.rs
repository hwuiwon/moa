use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::{TurnResult, build_default_pipeline, run_brain_turn};
use moa_core::{
    CompletionRequest, CompletionResponse, CompletionStream, Event, EventFilter, EventRange,
    EventRecord, LLMProvider, MemoryPath, MemoryScope, MemorySearchResult, MemoryStore, MoaConfig,
    ModelCapabilities, PageSummary, PageType, Result, SequenceNum, SessionFilter, SessionId,
    SessionMeta, SessionStatus, SessionStore, SessionSummary, StopReason, TokenPricing,
    ToolCallFormat, UserId, WikiPage, WorkspaceId,
};
use tokio::sync::Mutex;

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
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        Ok(self
            .events
            .lock()
            .await
            .iter()
            .filter(|record| record.session_id == session_id)
            .filter(|record| {
                range
                    .from_seq
                    .map(|from_seq| record.sequence_num >= from_seq)
                    .unwrap_or(true)
            })
            .filter(|record| {
                range
                    .to_seq
                    .map(|to_seq| record.sequence_num <= to_seq)
                    .unwrap_or(true)
            })
            .cloned()
            .collect())
    }

    async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
        Ok(self.session.lock().await.clone())
    }

    async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
        self.session.lock().await.status = status;
        Ok(())
    }

    async fn search_events(&self, _query: &str, _filter: EventFilter) -> Result<Vec<EventRecord>> {
        Ok(Vec::new())
    }

    async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct MockMemoryStore;

#[async_trait]
impl MemoryStore for MockMemoryStore {
    async fn search(
        &self,
        _query: &str,
        _scope: MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Ok(Vec::new())
    }

    async fn read_page(&self, _path: &MemoryPath) -> Result<WikiPage> {
        panic!("brain turn test does not expect memory reads")
    }

    async fn write_page(&self, _path: &MemoryPath, _page: WikiPage) -> Result<()> {
        Ok(())
    }

    async fn delete_page(&self, _path: &MemoryPath) -> Result<()> {
        Ok(())
    }

    async fn list_pages(
        &self,
        _scope: MemoryScope,
        _filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>> {
        Ok(Vec::new())
    }

    async fn get_index(&self, _scope: MemoryScope) -> Result<String> {
        Ok(String::new())
    }

    async fn rebuild_search_index(&self, _scope: MemoryScope) -> Result<()> {
        Ok(())
    }
}

struct MockLlmProvider;

#[async_trait]
impl LLMProvider for MockLlmProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
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

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
        Ok(CompletionStream::from_response(CompletionResponse {
            text: "Hi there".to_string(),
            content: vec![moa_core::CompletionContent::Text("Hi there".to_string())],
            stop_reason: StopReason::EndTurn,
            model: "claude-sonnet-4-6".to_string(),
            input_tokens: 32,
            output_tokens: 8,
            cached_input_tokens: 0,
            duration_ms: 25,
        }))
    }
}

fn make_event_record(session_id: &SessionId, sequence_num: u64, event: Event) -> EventRecord {
    EventRecord {
        id: uuid::Uuid::new_v4(),
        session_id: session_id.clone(),
        sequence_num,
        event_type: event.event_type(),
        event,
        timestamp: Utc::now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    }
}

#[tokio::test]
async fn run_brain_turn_emits_brain_response_event() {
    let session = SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        model: "claude-sonnet-4-6".to_string(),
        ..SessionMeta::default()
    };
    let initial_events = vec![make_event_record(
        &session.id,
        0,
        Event::UserMessage {
            text: "Hello".to_string(),
            attachments: Vec::new(),
        },
    )];
    let store = Arc::new(MockSessionStore::new(session.clone(), initial_events));
    let pipeline = build_default_pipeline(
        &MoaConfig::default(),
        store.clone(),
        Arc::new(MockMemoryStore),
    );
    let llm = Arc::new(MockLlmProvider);

    let result = run_brain_turn(session.id.clone(), store.clone(), llm, &pipeline)
        .await
        .unwrap();

    assert_eq!(result, TurnResult::Complete);

    let events = store.events.lock().await.clone();
    assert_eq!(events.len(), 2);
    match &events[1].event {
        Event::BrainResponse {
            text,
            model,
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(text, "Hi there");
            assert_eq!(model, "claude-sonnet-4-6");
            assert_eq!(*input_tokens, 32);
            assert_eq!(*output_tokens, 8);
        }
        other => panic!("expected brain response event, got {other:?}"),
    }
}
