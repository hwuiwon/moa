//! Tests for the session-store Restate facade.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use moa_core::{
    ContactId, ContactRef, ContactVerificationState, Event, EventFilter, EventRange, ModelId,
    SessionActorRef, SessionMeta, SessionStatus, TenantId,
    traits::{Identity, IdentityType},
};
use moa_session::testing;
use restate_sdk::prelude::HandlerError;
use uuid::Uuid;

use super::inner::create_session_for_identity;
use super::{
    AppendEventRequest, GetEventsRequest, SearchEventsRequest, SessionStoreImpl,
    UpdateStatusRequest,
};

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzOutboxTuple {
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    tenant_id: Option<Uuid>,
}

fn test_session_meta(workspace_id: &str) -> SessionMeta {
    let _ = workspace_id;
    SessionMeta {
        tenant_id: TenantId::new(),
        created_by: Some(SessionActorRef::Identity {
            id: Uuid::from_u128(1),
        }),
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

async fn install_authz_outbox(service: &SessionStoreImpl) -> Result<()> {
    let schema_name = service
        .store
        .schema_name()
        .context("test service should use an isolated schema")?;
    moa_migrations::run_auth_schema(service.store.pool(), schema_name).await?;
    Ok(())
}

#[tokio::test]
async fn create_session_for_identity_db_enqueues_owner_and_tenant_tuples() -> Result<()> {
    // Pins: authorized session creation relies on session#participant being derived from owner/tenant.
    let (service, database_url, schema_name) = test_service().await?;
    install_authz_outbox(&service).await?;
    let meta = test_session_meta("authorized-helper");
    let tenant_id = meta.tenant_id;
    let identity = Identity {
        identity_type: IdentityType::User,
        id: Uuid::new_v4(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let session_id = create_session_for_identity(service.store.as_ref(), meta, identity.clone())
        .await
        .map_err(into_anyhow)?;
    let session_object = format!("session:{session_id}");

    let tuples = sqlx::query_as::<_, AuthzOutboxTuple>(
        r#"
        SELECT tuple_user, tuple_relation, tuple_object, tenant_id
        FROM authz_outbox
        WHERE tuple_object = $1
        ORDER BY tuple_relation, tuple_user
        "#,
    )
    .bind(&session_object)
    .fetch_all(service.store.pool())
    .await?;

    assert_eq!(
        tuples,
        vec![
            AuthzOutboxTuple {
                tuple_user: format!("user:{}", identity.id),
                tuple_relation: "owner".to_string(),
                tuple_object: session_object.clone(),
                tenant_id: Some(identity.tenant_id.0),
            },
            AuthzOutboxTuple {
                tuple_user: format!("tenant:{tenant_id}"),
                tuple_relation: "tenant".to_string(),
                tuple_object: session_object,
                tenant_id: Some(identity.tenant_id.0),
            },
        ]
    );

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn create_contact_session_for_identity_db_enqueues_contact_session_tuple() -> Result<()> {
    // Pins: contact-backed sessions enqueue the session#contact tuple that grants contact participation.
    let (service, database_url, schema_name) = test_service().await?;
    install_authz_outbox(&service).await?;
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let meta = SessionMeta {
        tenant_id,
        contact: Some(ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::json!({}),
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    };
    let identity = Identity {
        identity_type: IdentityType::Contact,
        id: contact_id.0,
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let session_id = create_session_for_identity(service.store.as_ref(), meta, identity.clone())
        .await
        .map_err(into_anyhow)?;
    let session_object = format!("session:{session_id}");

    let tuples = sqlx::query_as::<_, AuthzOutboxTuple>(
        r#"
        SELECT tuple_user, tuple_relation, tuple_object, tenant_id
        FROM authz_outbox
        WHERE tuple_object = $1
        ORDER BY tuple_relation, tuple_user
        "#,
    )
    .bind(&session_object)
    .fetch_all(service.store.pool())
    .await?;

    assert!(tuples.contains(&AuthzOutboxTuple {
        tuple_user: format!("contact:{contact_id}"),
        tuple_relation: "contact".to_string(),
        tuple_object: session_object.clone(),
        tenant_id: Some(tenant_id.0),
    }));
    assert!(tuples.contains(&AuthzOutboxTuple {
        tuple_user: format!("contact:{contact_id}"),
        tuple_relation: "owner".to_string(),
        tuple_object: session_object.clone(),
        tenant_id: Some(tenant_id.0),
    }));
    assert!(tuples.contains(&AuthzOutboxTuple {
        tuple_user: format!("tenant:{tenant_id}"),
        tuple_relation: "tenant".to_string(),
        tuple_object: session_object,
        tenant_id: Some(tenant_id.0),
    }));

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn append_event_db_increments_sequence() -> Result<()> {
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
async fn get_events_db_respects_range() -> Result<()> {
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
async fn update_status_db_affects_get_session() -> Result<()> {
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
async fn search_events_db_finds_by_payload() -> Result<()> {
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
