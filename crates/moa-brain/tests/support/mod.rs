//! Shared helpers for offline brain integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    Channel, ContextMessage, Event, EventFilter, EventRange, EventRecord, ModelCapabilities,
    ModelId, Result, SequenceNum, SessionFilter, SessionId, SessionMeta, SessionStatus,
    SessionStore, SessionSummary, TokenPricing, ToolCallFormat, UserId, WorkingContext,
    WorkspaceId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

const DEFAULT_CLOCK: &str = "2026-05-07T12:00:00Z";
const WORKSPACE_ROOT_METADATA_KEY: &str = "_moa.runtime.workspace_root";

/// Builds a deterministic [`WorkingContext`] fixture for one-stage pipeline tests.
pub struct WorkingContextFixture {
    workspace_id: String,
    user_id: String,
    model_id: String,
    tool_names: Vec<String>,
    memory_hits: Vec<MemoryHitSpec>,
    messages: Vec<ContextMessage>,
    clock_at: DateTime<Utc>,
}

impl WorkingContextFixture {
    /// Creates a fixture with a stable workspace, user, model, clock, and one user message.
    #[must_use]
    pub fn new() -> Self {
        Self {
            workspace_id: "ws-fixture".to_string(),
            user_id: "user-fixture".to_string(),
            model_id: "claude-sonnet-4-6".to_string(),
            tool_names: Vec::new(),
            memory_hits: Vec::new(),
            messages: vec![ContextMessage::user("fix the auth bug")],
            clock_at: parse_utc(DEFAULT_CLOCK),
        }
    }

    /// Sets the workspace id used by the compiled context and workspace-root fixture.
    #[must_use]
    pub fn with_workspace_id(mut self, workspace_id: &str) -> Self {
        self.workspace_id = workspace_id.to_string();
        self
    }

    /// Sets the user id used by the compiled context.
    #[must_use]
    pub fn with_user_id(mut self, user_id: &str) -> Self {
        self.user_id = user_id.to_string();
        self
    }

    /// Sets the active model id used by the compiled context.
    #[must_use]
    pub fn with_model_id(mut self, model_id: &str) -> Self {
        self.model_id = model_id.to_string();
        self
    }

    /// Sets the available tool names and preloads matching deterministic tool schemas.
    #[must_use]
    pub fn with_tools(mut self, tools: &[&str]) -> Self {
        self.tool_names = tools.iter().map(|tool| (*tool).to_string()).collect();
        self
    }

    /// Sets memory-hit specs assigned stable UUIDs when the fixture is built.
    #[must_use]
    pub fn with_memory_hits(mut self, hits: &[MemoryHitSpec]) -> Self {
        self.memory_hits = hits.to_vec();
        self
    }

    /// Replaces the fixture conversation with one user message.
    #[must_use]
    pub fn with_user_message(mut self, message: &str) -> Self {
        self.messages = vec![ContextMessage::user(message)];
        self
    }

    /// Replaces the fixture conversation with explicit context messages.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<ContextMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Sets the deterministic clock timestamp used by runtime-stage tests.
    #[must_use]
    pub fn with_clock_at(mut self, timestamp: &str) -> Self {
        self.clock_at = parse_utc(timestamp);
        self
    }

    /// Builds the deterministic context fixture.
    #[must_use]
    pub fn build(self) -> BuiltWorkingContextFixture {
        let tempdir = tempfile::tempdir().expect("pipeline fixture tempdir should be created");
        let workspace_root = tempdir.path().join(&self.workspace_id);
        let session = SessionMeta {
            id: SessionId(Uuid::from_u128(0x100)),
            workspace_id: WorkspaceId::new(self.workspace_id.clone()),
            user_id: UserId::new(self.user_id.clone()),
            title: None,
            status: SessionStatus::Created,
            channel: Channel::Chat,
            active_channel_binding_id: None,
            model: ModelId::new(self.model_id.clone()),
            created_at: self.clock_at,
            updated_at: self.clock_at,
            completed_at: None,
            parent_session_id: None,
            contact: None,
            created_by: None,
            contact_promoted_from_id: None,
            total_input_tokens: 0,
            total_input_tokens_uncached: 0,
            total_input_tokens_cache_write: 0,
            total_input_tokens_cache_read: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        };
        let mut ctx = WorkingContext::new(&session, capabilities(&self.model_id));
        ctx.insert_metadata(
            WORKSPACE_ROOT_METADATA_KEY,
            json!(workspace_root.display().to_string()),
        );
        ctx.set_tools(
            self.tool_names
                .iter()
                .map(|name| tool_schema(name))
                .collect(),
        );
        ctx.extend_messages(self.messages);
        let memory_hits = self
            .memory_hits
            .into_iter()
            .enumerate()
            .map(|(index, spec)| spec.into_hit(index))
            .collect();

        BuiltWorkingContextFixture {
            ctx,
            tool_schemas: self
                .tool_names
                .iter()
                .map(|name| tool_schema(name))
                .collect(),
            memory_hits,
            clock_at: self.clock_at,
            workspace_root,
            _tempdir: tempdir,
        }
    }
}

impl Default for WorkingContextFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Built deterministic context plus fixture-side inputs used by individual stages.
pub struct BuiltWorkingContextFixture {
    /// Mutable working context under test.
    pub ctx: WorkingContext,
    /// Deterministic tool schemas corresponding to the requested tool names.
    pub tool_schemas: Vec<Value>,
    /// Deterministic memory hits corresponding to the requested hit specs.
    pub memory_hits: Vec<MemoryHit>,
    /// Fixed clock instant used by runtime-stage tests.
    pub clock_at: DateTime<Utc>,
    /// Workspace root path recorded in context metadata.
    pub workspace_root: PathBuf,
    _tempdir: TempDir,
}

/// One deterministic memory hit specification before UUID assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHitSpec {
    name: String,
    summary: String,
    valid: bool,
}

impl MemoryHitSpec {
    /// Marks the memory hit as invalidated before retrieval.
    #[must_use]
    pub fn invalidated(mut self) -> Self {
        self.valid = false;
        self
    }

    fn into_hit(self, index: usize) -> MemoryHit {
        MemoryHit {
            uid: Uuid::from_u128(0x1_000 + index as u128),
            name: self.name,
            summary: self.summary,
            valid: self.valid,
        }
    }
}

/// Creates a valid deterministic memory-hit fixture.
#[must_use]
pub fn mem_hit(name: &str, summary: &str) -> MemoryHitSpec {
    MemoryHitSpec {
        name: name.to_string(),
        summary: summary.to_string(),
        valid: true,
    }
}

/// One deterministic memory hit with an assigned UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHit {
    /// Stable graph-memory UID.
    pub uid: Uuid,
    /// Human-readable node name.
    pub name: String,
    /// Summary inserted into `properties_summary`.
    pub summary: String,
    /// Whether the node is active for retrieval.
    pub valid: bool,
}

/// Creates a deterministic tool schema for the named tool.
#[must_use]
pub fn tool_schema(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("Deterministic fixture tool {name}"),
        "input_schema": {
            "type": "object",
            "properties": {
                "input": { "type": "string" }
            },
            "required": ["input"]
        }
    })
}

/// Returns deterministic model capabilities for context fixture tests.
#[must_use]
pub fn capabilities(model_id: &str) -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new(model_id),
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

/// In-memory session store for brain harness tests that do not need Postgres.
#[derive(Clone)]
pub struct MockSessionStore {
    session: Arc<Mutex<SessionMeta>>,
    events: Arc<Mutex<Vec<EventRecord>>>,
}

impl MockSessionStore {
    /// Creates a store with the provided initial session metadata and events.
    pub fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            events: Arc::new(Mutex::new(events)),
        }
    }

    /// Returns all stored events for assertions.
    pub async fn all_events(&self) -> Vec<EventRecord> {
        self.events.lock().await.clone()
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

    async fn workspace_cost_since(
        &self,
        workspace_id: &WorkspaceId,
        since: DateTime<Utc>,
    ) -> Result<u32> {
        let session = self.session.lock().await.clone();
        if &session.workspace_id != workspace_id {
            return Ok(0);
        }

        Ok(self
            .events
            .lock()
            .await
            .iter()
            .filter(|record| record.timestamp >= since)
            .filter_map(|record| match &record.event {
                Event::BrainResponse { cost_cents, .. } => Some(*cost_cents),
                _ => None,
            })
            .sum())
    }

    async fn delete_empty_session(&self, _session_id: SessionId) -> Result<()> {
        Ok(())
    }
}

/// Builds session metadata for an offline brain test.
pub fn session_meta(label: &str, model: &str) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new(format!("{label}-workspace")),
        user_id: moa_core::UserId::new(format!("{label}-user")),
        model: ModelId::new(model),
        ..SessionMeta::default()
    }
}

/// Mounts a wiremock OpenAI Responses stream that returns the supplied text.
pub async fn mount_openai_text(server: &MockServer, text: impl Into<String>, cached_tokens: usize) {
    Mock::given(any())
        .respond_with(openai_text_response(text.into(), cached_tokens))
        .mount(server)
        .await;
}

/// Returns captured request bodies as JSON values.
pub async fn captured_json_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests")
        .into_iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect()
}

fn openai_text_response(text: String, cached_tokens: usize) -> ResponseTemplate {
    let events = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_offline",
                "object": "response",
                "created_at": 1,
                "model": "gpt-5.4",
                "output": [],
                "status": "in_progress"
            }
        }),
        json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": text,
            "logprobs": null
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp_offline",
                "object": "response",
                "created_at": 1,
                "completed_at": 2,
                "model": "gpt-5.4",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                        "logprobs": null
                    }]
                }],
                "status": "completed",
                "usage": {
                    "input_tokens": 16,
                    "input_tokens_details": { "cached_tokens": cached_tokens },
                    "output_tokens": 4,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 20
                }
            }
        }),
    ];
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();

    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .insert_header("cache-control", "no-cache")
        .set_body_raw(body, "text/event-stream")
}

fn parse_utc(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .expect("fixture timestamp should be RFC3339")
        .with_timezone(&Utc)
}
