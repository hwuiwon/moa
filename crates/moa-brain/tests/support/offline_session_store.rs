//! In-memory session-store helpers for offline brain tests.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    ContactId, ContactRef, ContactVerificationState, Event, EventFilter, EventRange, EventRecord,
    ModelId, Result, SequenceNum, SessionActorRef, SessionFilter, SessionId, SessionMeta,
    SessionStatus, SessionStore, SessionSummary, TenantId,
};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

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
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}
