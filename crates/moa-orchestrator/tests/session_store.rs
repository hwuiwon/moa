//! Postgres-backed session-store contract coverage for the orchestrator crate.

use anyhow::Result;
use moa_core::{
    Event, EventFilter, EventRange, ModelId, SessionMeta, SessionStatus, SessionStore, UserId,
    WorkspaceId,
};
use moa_session::{PostgresSessionStore, testing};

fn test_session_meta(workspace_id: &str) -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new(workspace_id),
        user_id: UserId::new("user-1"),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
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
