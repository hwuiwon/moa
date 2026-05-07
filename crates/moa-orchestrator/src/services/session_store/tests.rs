//! Tests for the session-store Restate facade.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use moa_core::{
    Event, EventFilter, EventRange, ModelId, SessionMeta, SessionStatus, UserId, WorkspaceId,
};
use moa_session::testing;
use restate_sdk::prelude::HandlerError;

use super::{
    AppendEventRequest, GetEventsRequest, SearchEventsRequest, SessionStoreImpl,
    UpdateStatusRequest,
};

fn test_session_meta(workspace_id: &str) -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new(workspace_id),
        user_id: UserId::new("user-1"),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

async fn test_service() -> Result<(SessionStoreImpl, String, String)> {
    let (store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    Ok((
        SessionStoreImpl::new(Arc::new(store)),
        database_url,
        schema_name,
    ))
}

async fn cleanup(database_url: &str, schema_name: &str) -> Result<()> {
    testing::cleanup_test_schema(database_url, schema_name).await?;
    Ok(())
}

fn into_anyhow(error: HandlerError) -> anyhow::Error {
    anyhow!("{error:?}")
}

#[tokio::test]
async fn append_event_increments_sequence() -> Result<()> {
    let (service, database_url, schema_name) = test_service().await?;
    let session_id = service
        .create_session_inner(test_session_meta("append-seq"))
        .await
        .map_err(into_anyhow)?;

    let seq0 = service
        .append_event_inner(AppendEventRequest {
            session_id,
            event: Event::UserMessage {
                text: "first".to_string(),
                attachments: vec![],
            },
        })
        .await
        .map_err(into_anyhow)?;
    let seq1 = service
        .append_event_inner(AppendEventRequest {
            session_id,
            event: Event::UserMessage {
                text: "second".to_string(),
                attachments: vec![],
            },
        })
        .await
        .map_err(into_anyhow)?;
    let seq2 = service
        .append_event_inner(AppendEventRequest {
            session_id,
            event: Event::UserMessage {
                text: "third".to_string(),
                attachments: vec![],
            },
        })
        .await
        .map_err(into_anyhow)?;

    assert_eq!((seq0, seq1, seq2), (0, 1, 2));

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn get_events_respects_range() -> Result<()> {
    let (service, database_url, schema_name) = test_service().await?;
    let session_id = service
        .create_session_inner(test_session_meta("range"))
        .await
        .map_err(into_anyhow)?;

    for index in 0..10 {
        service
            .append_event_inner(AppendEventRequest {
                session_id,
                event: Event::UserMessage {
                    text: format!("message {index}"),
                    attachments: vec![],
                },
            })
            .await
            .map_err(into_anyhow)?;
    }

    let events = service
        .get_events_inner(GetEventsRequest {
            session_id,
            range: EventRange {
                from_seq: Some(3),
                to_seq: Some(7),
                event_types: None,
                limit: None,
            },
        })
        .await
        .map_err(into_anyhow)?;

    assert_eq!(events.len(), 5);
    assert_eq!(events.first().map(|record| record.sequence_num), Some(3));
    assert_eq!(events.last().map(|record| record.sequence_num), Some(7));

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn update_status_affects_get_session() -> Result<()> {
    let (service, database_url, schema_name) = test_service().await?;
    let session_id = service
        .create_session_inner(test_session_meta("status"))
        .await
        .map_err(into_anyhow)?;

    service
        .update_status_inner(UpdateStatusRequest {
            session_id,
            status: SessionStatus::Completed,
        })
        .await
        .map_err(into_anyhow)?;
    let session = service
        .get_session_inner(session_id)
        .await
        .map_err(into_anyhow)?;

    assert_eq!(session.status, SessionStatus::Completed);
    assert!(session.completed_at.is_some());

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn search_events_finds_by_payload() -> Result<()> {
    let (service, database_url, schema_name) = test_service().await?;
    let session_id = service
        .create_session_inner(test_session_meta("search"))
        .await
        .map_err(into_anyhow)?;

    service
        .append_event_inner(AppendEventRequest {
            session_id,
            event: Event::UserMessage {
                text: "Fix the OAuth refresh token bug".to_string(),
                attachments: vec![],
            },
        })
        .await
        .map_err(into_anyhow)?;
    service
        .append_event_inner(AppendEventRequest {
            session_id,
            event: Event::UserMessage {
                text: "Debug the refresh-token rotation failure".to_string(),
                attachments: vec![],
            },
        })
        .await
        .map_err(into_anyhow)?;

    let events = service
        .search_events_inner(SearchEventsRequest {
            query: "refresh-token".to_string(),
            filter: EventFilter::default(),
        })
        .await
        .map_err(into_anyhow)?;

    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::UserMessage { text, .. } if text.contains("refresh-token")
    )));

    cleanup(&database_url, &schema_name).await
}
