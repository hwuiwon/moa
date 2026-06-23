//! Postgres-backed session-store contract coverage for the orchestrator crate.

use anyhow::Result;
use moa_core::{
    ContactId, ContactRef, ContactVerificationState, Event, EventFilter, EventRange, ModelId,
    SessionActorRef, SessionFilter, SessionMeta, SessionStatus, SessionStore, TenantId,
};
use moa_session::{PostgresSessionStore, testing};
use uuid::Uuid;

fn test_session_meta(_workspace_id: &str) -> SessionMeta {
    SessionMeta {
        tenant_id: TenantId::from(
            Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                .expect("fixture tenant id parses"),
        ),
        created_by: Some(SessionActorRef::Identity {
            id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                .expect("fixture identity id parses"),
        }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: vec!["agent:session:create".to_string()],
        permissions: serde_json::json!({}),
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}

async fn test_store() -> Result<(PostgresSessionStore, String, String)> {
    testing::create_isolated_test_store()
        .await
        .map_err(Into::into)
}

async fn cleanup(database_url: &str, schema_name: &str) -> Result<()> {
    testing::cleanup_test_schema(database_url, schema_name)
        .await
        .map_err(Into::into)
}

#[tokio::test]
async fn create_session_persists_requested_metadata() -> Result<()> {
    // Pins: the core Postgres session-store create path remains a metadata-row write.
    let (store, database_url, schema_name) = test_store().await?;
    let meta = test_session_meta("create-metadata");
    let session_id = store.create_session(meta.clone()).await?;

    let persisted = store.get_session(session_id).await?;

    assert_eq!(persisted.id, meta.id);
    assert_eq!(persisted.tenant_id, meta.tenant_id);
    assert_eq!(persisted.created_by, meta.created_by);
    assert_eq!(persisted.model, meta.model);
    assert_eq!(persisted.status, meta.status);

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn append_event_increments_sequence() -> Result<()> {
    let (store, database_url, schema_name) = test_store().await?;
    let session_id = store
        .create_session(test_session_meta("append-seq"))
        .await?;

    let seq0 = store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "first".to_string(),
                attachments: vec![],
            },
        )
        .await?;
    let seq1 = store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "second".to_string(),
                attachments: vec![],
            },
        )
        .await?;
    let seq2 = store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "third".to_string(),
                attachments: vec![],
            },
        )
        .await?;

    assert_eq!((seq0, seq1, seq2), (0, 1, 2));

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn get_events_respects_range() -> Result<()> {
    let (store, database_url, schema_name) = test_store().await?;
    let session_id = store.create_session(test_session_meta("range")).await?;

    for index in 0..10 {
        store
            .emit_event(
                session_id,
                Event::UserMessage {
                    text: format!("message {index}"),
                    attachments: vec![],
                },
            )
            .await?;
    }

    let events = store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(3),
                to_seq: Some(7),
                event_types: None,
                limit: None,
            },
        )
        .await?;

    assert_eq!(events.len(), 5);
    assert_eq!(events.first().map(|record| record.sequence_num), Some(3));
    assert_eq!(events.last().map(|record| record.sequence_num), Some(7));

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn tenant_admin_can_read_contact_session_db() -> Result<()> {
    // Pins: tenant-wide session inspection can list and load contact sessions without crossing tenants.
    let (store, database_url, schema_name) = test_store().await?;
    let tenant_id = TenantId::from(
        Uuid::parse_str("33333333-3333-4333-8333-333333333333").expect("fixture tenant id parses"),
    );
    let other_tenant_id = TenantId::from(
        Uuid::parse_str("44444444-4444-4444-8444-444444444444").expect("fixture tenant id parses"),
    );
    let contact_id = ContactId::new();
    let session_id = store
        .create_session(SessionMeta {
            tenant_id,
            contact: Some(contact_ref(tenant_id, contact_id)),
            created_by: Some(SessionActorRef::Contact { id: contact_id }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await?;
    store
        .create_session(SessionMeta {
            tenant_id: other_tenant_id,
            contact: Some(contact_ref(other_tenant_id, ContactId::new())),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::parse_str("55555555-5555-4555-8555-555555555555")
                    .expect("fixture identity id parses"),
            }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await?;

    let summaries = store
        .list_sessions(SessionFilter {
            tenant_id: Some(tenant_id),
            ..SessionFilter::default()
        })
        .await?;
    let persisted = store.get_session(session_id).await?;

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session_id, session_id);
    assert_eq!(summaries[0].tenant_id, tenant_id);
    assert_eq!(
        summaries[0]
            .contact
            .as_ref()
            .map(|contact| contact.contact_id),
        Some(contact_id)
    );
    assert_eq!(persisted.tenant_id, tenant_id);
    assert_eq!(
        persisted.contact.as_ref().map(|contact| contact.contact_id),
        Some(contact_id)
    );

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn update_status_affects_get_session() -> Result<()> {
    let (store, database_url, schema_name) = test_store().await?;
    let session_id = store.create_session(test_session_meta("status")).await?;

    store
        .update_status(session_id, SessionStatus::Completed)
        .await?;
    let session = store.get_session(session_id).await?;

    assert_eq!(session.status, SessionStatus::Completed);
    assert!(session.completed_at.is_some());

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn search_events_finds_by_payload() -> Result<()> {
    let (store, database_url, schema_name) = test_store().await?;
    let session_id = store.create_session(test_session_meta("search")).await?;

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Fix the OAuth refresh token bug".to_string(),
                attachments: vec![],
            },
        )
        .await?;
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Debug the refresh-token rotation failure".to_string(),
                attachments: vec![],
            },
        )
        .await?;

    let events = store
        .search_events("refresh-token", EventFilter::default())
        .await?;

    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::UserMessage { text, .. } if text.contains("refresh-token")
    )));

    cleanup(&database_url, &schema_name).await
}
