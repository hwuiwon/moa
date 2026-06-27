//! Regression suite test fixtures.

use chrono::Utc;
use moa_core::{
    Attachment, Channel, Event, EventRecord, ModelId, ModelTier, SessionId, SessionMeta,
    SessionStatus, StoragePartitionId, TenantId, ToolCallId, ToolOutput,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Successful session fixture with exactly five tool calls.
pub const SESSION_WITH_5_TOOL_CALLS: &str = include_str!("fixtures/session_with_5_tool_calls.json");

#[derive(Debug, Deserialize)]
struct SessionFixture {
    session_id: Uuid,
    storage_partition_id: String,
    user_id: String,
    task: String,
    final_response: String,
    tool_calls: Vec<ToolCallFixture>,
}

#[derive(Debug, Deserialize)]
struct ToolCallFixture {
    tool_name: String,
    input: Value,
    output: String,
}

/// One parsed session fixture ready for distillation tests.
pub struct LoadedSession {
    /// Session metadata.
    pub session: SessionMeta,
    /// Event log records.
    pub events: Vec<EventRecord>,
}

/// Loads a session JSON fixture into typed session metadata and records.
pub fn load_session_fixture(json_text: &str) -> LoadedSession {
    let fixture: SessionFixture =
        serde_json::from_str(json_text).expect("parse skill session fixture");
    let storage_partition_id = StoragePartitionId::new(format!(
        "{}-{}",
        fixture.storage_partition_id,
        Uuid::now_v7().simple()
    ));
    let _user_id = fixture.user_id;
    let session = SessionMeta {
        id: SessionId(fixture.session_id),
        tenant_id: tenant_id_from_storage_partition(&storage_partition_id),
        title: Some(fixture.task.clone()),
        status: SessionStatus::Completed,
        channel: Channel::Chat,
        model: ModelId::new("scripted-skill-model"),
        ..SessionMeta::default()
    };
    let mut events = Vec::new();
    push_event(
        &mut events,
        session.id,
        Event::UserMessage {
            text: fixture.task,
            attachments: Vec::<Attachment>::new(),
        },
    );
    for tool_call in fixture.tool_calls {
        let tool_id = ToolCallId::new();
        push_event(
            &mut events,
            session.id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: tool_call.tool_name,
                input: tool_call.input,
                hand_id: None,
            },
        );
        push_event(
            &mut events,
            session.id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text(tool_call.output, std::time::Duration::from_millis(1)),
                original_output_tokens: None,
                success: true,
                duration_ms: 1,
            },
        );
    }
    push_event(
        &mut events,
        session.id,
        Event::BrainResponse {
            text: fixture.final_response,
            thought_signature: None,
            model: ModelId::new("scripted-skill-model"),
            model_tier: ModelTier::Auxiliary,
            input_tokens_uncached: 128,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 32,
            cost_cents: 0,
            duration_ms: 1,
        },
    );
    LoadedSession { session, events }
}

/// Returns a complete skill markdown document for ad hoc test skills.
pub fn skill_markdown(name: &str, description: &str, body: &str, version: &str) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: \"{description}\"\n\
         allowed-tools: bash file_search file_read\n\
         metadata:\n\
           moa-version: \"{version}\"\n\
           moa-tags: \"auth, regression\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {name}\n\n\
         {body}\n"
    )
}

fn push_event(events: &mut Vec<EventRecord>, session_id: SessionId, event: Event) {
    events.push(EventRecord {
        id: Uuid::now_v7(),
        session_id,
        sequence_num: events.len() as u64 + 1,
        event_type: event.event_type(),
        event,
        timestamp: Utc::now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    });
}

fn tenant_id_from_storage_partition(storage_partition_id: &StoragePartitionId) -> TenantId {
    if let Ok(uuid) = Uuid::parse_str(storage_partition_id.as_str()) {
        return TenantId::from(uuid);
    }
    let digest = Sha256::digest(storage_partition_id.as_str().as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
}
