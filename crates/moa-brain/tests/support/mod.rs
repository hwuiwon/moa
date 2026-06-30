//! Shared helpers for offline brain integration tests.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    Channel, ContactId, ContactRef, ContactVerificationState, ContextMessage, Event, EventFilter,
    EventRange, EventRecord, ModelCapabilities, ModelId, Result, SequenceNum, SessionActorRef,
    SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore, SessionSummary, TenantId,
    TokenPricing, ToolCallFormat, WorkingContext,
};
use moa_test_support::fixtures::contact_ref_fixture;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

const DEFAULT_CLOCK: &str = "2026-05-07T12:00:00Z";
const WORKSPACE_ROOT_METADATA_KEY: &str = "_moa.runtime.workspace_root";

/// Builds a deterministic [`WorkingContext`] fixture for one-stage pipeline tests.
pub struct WorkingContextFixture {
    storage_partition_id: String,
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
            storage_partition_id: "ws-fixture".to_string(),
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
    pub fn with_storage_partition_id(mut self, storage_partition_id: &str) -> Self {
        self.storage_partition_id = storage_partition_id.to_string();
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
        let workspace_root = tempdir.path().join(&self.storage_partition_id);
        let tenant_id = tenant_id_from_label(&self.storage_partition_id);
        let contact_id = contact_id_from_label(&self.user_id);
        let session = SessionMeta {
            id: SessionId(Uuid::from_u128(0x100)),
            tenant_id,
            title: None,
            status: SessionStatus::Created,
            channel: Channel::Chat,
            active_channel_binding_id: None,
            model: ModelId::new(self.model_id.clone()),
            created_at: self.clock_at,
            updated_at: self.clock_at,
            completed_at: None,
            parent_session_id: None,
            contact: Some(contact_ref(tenant_id, contact_id)),
            created_by: Some(SessionActorRef::Contact { id: contact_id }),
            contact_promoted_from_id: None,
            agent_context: Some(moa_core::AgentContext::system_default()),
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

    async fn tenant_cost_since(&self, tenant_id: &TenantId, since: DateTime<Utc>) -> Result<u32> {
        let session = self.session.lock().await.clone();
        if session.tenant_id != *tenant_id {
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
    let workspace_label = format!("{label}-workspace");
    let user_label = format!("{label}-user");
    let tenant_id = tenant_id_from_label(&workspace_label);
    let contact_id = contact_id_from_label(&user_label);
    SessionMeta {
        id: SessionId::new(),
        tenant_id,
        contact: Some(contact_ref(tenant_id, contact_id)),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model: ModelId::new(model),
        ..SessionMeta::default()
    }
}

fn tenant_id_from_label(label: &str) -> TenantId {
    Uuid::parse_str(label)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(label)))
}

fn contact_id_from_label(label: &str) -> ContactId {
    Uuid::parse_str(label)
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(label)))
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let hash = blake3::hash(label.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    contact_ref_fixture(contact_id, tenant_id, ContactVerificationState::Verified)
}

fn parse_utc(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .expect("fixture timestamp should be RFC3339")
        .with_timezone(&Utc)
}
