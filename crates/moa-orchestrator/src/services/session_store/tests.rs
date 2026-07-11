//! Tests for the session-store Restate facade.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use moa_core::{
    events::Event,
    traits::SessionStore,
    traits::{Identity, IdentityType, SessionChannelBindingUpdate},
    types::channel::ChannelRef,
    types::channel::SessionChannelBindingId,
    types::contact::ContactId,
    types::contact::ContactVerificationState,
    types::contact::SessionActorRef,
    types::events_stream::EventFilter,
    types::events_stream::EventRange,
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::session::SessionMeta,
    types::session::SessionStatus,
};
use moa_session::testing;
use moa_test_support::fixtures::{contact_ref_fixture, session_meta_fixture};
use uuid::Uuid;

use super::SessionStoreImpl;
use super::inner::{
    change_contact_session_channel_atomic, create_session_for_identity,
    initialize_contact_session_atomic,
};

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzOutboxTuple {
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    tenant_id: Option<Uuid>,
}

fn test_session_meta(storage_partition_id: &str) -> SessionMeta {
    let _ = storage_partition_id;
    session_meta_fixture(TenantId::new())
}

async fn test_service() -> Result<(SessionStoreImpl, String, String)> {
    let (store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    Ok((
        SessionStoreImpl::new(Arc::new(store), pool),
        database_url,
        schema_name,
    ))
}

async fn cleanup(database_url: &str, schema_name: &str) -> Result<()> {
    testing::cleanup_test_schema(database_url, schema_name).await?;
    Ok(())
}

fn into_anyhow(error: impl std::fmt::Debug) -> anyhow::Error {
    anyhow!("{error:?}")
}

async fn install_authz_outbox(service: &SessionStoreImpl) -> Result<()> {
    let schema_name = service
        .store
        .schema_name()
        .context("test service should use an isolated schema")?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
            FROM information_schema.tables
            WHERE table_schema = $1 AND table_name = 'authz_outbox'
        )",
    )
    .bind(schema_name)
    .fetch_one(service.store.pool())
    .await?;
    if exists {
        return Ok(());
    }
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
        identity_type: IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let session_id = create_session_for_identity(
        service.store.as_ref(),
        &service.pool,
        meta,
        identity.clone(),
    )
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
                tuple_user: format!("operator:{}", identity.id),
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
    let mut contact =
        contact_ref_fixture(contact_id, tenant_id, ContactVerificationState::Verified);
    contact.permissions = serde_json::json!({});
    let meta = SessionMeta {
        tenant_id,
        contact: Some(contact),
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
    let session_id = create_session_for_identity(
        service.store.as_ref(),
        &service.pool,
        meta,
        identity.clone(),
    )
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

fn contact_session_meta(
    tenant_id: TenantId,
    contact_id: ContactId,
    session_id: SessionId,
) -> SessionMeta {
    let contact = contact_ref_fixture(contact_id, tenant_id, ContactVerificationState::Verified);
    SessionMeta {
        id: session_id,
        tenant_id,
        contact: Some(contact),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

fn contact_identity_for(tenant_id: TenantId, contact_id: ContactId) -> Identity {
    Identity {
        identity_type: IdentityType::Contact,
        id: contact_id.0,
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

/// Inserts the verified `contacts` row that the channel-binding foreign key
/// requires (production creates it during contact token issuance).
async fn insert_verified_contact(
    service: &SessionStoreImpl,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO contacts (id, tenant_id, storage_partition_id, contact_id, state) \
         VALUES ($1, $2, $3, $4, 'verified')",
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(contact_id.0)
    .execute(service.store.pool())
    .await?;
    Ok(())
}

fn slack_channel_ref(thread_ts: &str) -> ChannelRef {
    ChannelRef::Slack {
        team_id: Some("T123".to_string()),
        slack_channel_id: Some("C123".to_string()),
        thread_ts: Some(thread_ts.to_string()),
        user_id: Some("U123".to_string()),
    }
}

fn binding_update(
    tenant_id: TenantId,
    contact_id: ContactId,
    session_id: SessionId,
    channel_ref: &ChannelRef,
) -> SessionChannelBindingUpdate {
    SessionChannelBindingUpdate {
        tenant_id,
        storage_partition_id: StoragePartitionId::for_tenant(tenant_id),
        session_id,
        contact_id,
        channel_account_id: None,
        contact_point_id: None,
        channel_ref: channel_ref.clone(),
        reason: Some("test".to_string()),
    }
}

fn session_created_event(
    tenant_id: TenantId,
    contact_id: ContactId,
    channel_ref: &ChannelRef,
) -> Event {
    Event::SessionCreated {
        tenant_id,
        contact_id: Some(contact_id),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model: ModelId::new("test-model"),
        channel: channel_ref.channel(),
    }
}

async fn count_scalar(service: &SessionStoreImpl, sql: &str, bind: Uuid) -> Result<i64> {
    Ok(sqlx::query_scalar(sql)
        .bind(bind)
        .fetch_one(service.store.pool())
        .await?)
}

async fn count_sessions(service: &SessionStoreImpl, session_id: SessionId) -> Result<i64> {
    count_scalar(
        service,
        "SELECT COUNT(*) FROM sessions WHERE id = $1",
        session_id.0,
    )
    .await
}

async fn count_events(service: &SessionStoreImpl, session_id: SessionId) -> Result<i64> {
    count_scalar(
        service,
        "SELECT COUNT(*) FROM events WHERE session_id = $1",
        session_id.0,
    )
    .await
}

async fn count_active_bindings(service: &SessionStoreImpl, session_id: SessionId) -> Result<i64> {
    count_scalar(
        service,
        "SELECT COUNT(*) FROM session_channel_bindings WHERE session_id = $1 AND ended_at IS NULL",
        session_id.0,
    )
    .await
}

async fn count_session_tuples(service: &SessionStoreImpl, session_id: SessionId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM authz_outbox WHERE tuple_object = $1")
            .bind(format!("session:{session_id}"))
            .fetch_one(service.store.pool())
            .await?,
    )
}

#[tokio::test]
async fn initialize_contact_session_atomic_is_idempotent_on_replay_db() -> Result<()> {
    // Pins: replaying contact session creation with the same stable id inserts no
    // second session, binding, SessionCreated event, or authz outbox tuple.
    let (service, database_url, schema_name) = test_service().await?;
    install_authz_outbox(&service).await?;
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session_id = SessionId::new();
    insert_verified_contact(&service, tenant_id, contact_id).await?;
    let meta = contact_session_meta(tenant_id, contact_id, session_id);
    let identity = contact_identity_for(tenant_id, contact_id);
    let channel_ref = slack_channel_ref("1712668800.000100");

    // First creation, then a handler replay that reuses the same session id but
    // (as a real replay would) allocates a fresh binding id inside the closure.
    for binding_id in [
        SessionChannelBindingId::new(),
        SessionChannelBindingId::new(),
    ] {
        initialize_contact_session_atomic(
            service.store.as_ref(),
            &service.pool,
            meta.clone(),
            identity.clone(),
            binding_id,
            binding_update(tenant_id, contact_id, session_id, &channel_ref),
            session_created_event(tenant_id, contact_id, &channel_ref),
        )
        .await
        .map_err(into_anyhow)?;
    }

    assert_eq!(count_sessions(&service, session_id).await?, 1);
    assert_eq!(count_events(&service, session_id).await?, 1);
    assert_eq!(count_active_bindings(&service, session_id).await?, 1);
    assert_eq!(
        count_session_tuples(&service, session_id).await?,
        3,
        "owner, tenant, and contact tuples are enqueued exactly once"
    );

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn initialize_contact_session_atomic_rolls_back_on_late_failure_db() -> Result<()> {
    // Pins: a failure after the session insert (the binding targets a session id
    // that does not exist) rolls back the whole product transaction, leaving no
    // orphan session row, event, or authz tuple.
    let (service, database_url, schema_name) = test_service().await?;
    install_authz_outbox(&service).await?;
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session_id = SessionId::new();
    insert_verified_contact(&service, tenant_id, contact_id).await?;
    let meta = contact_session_meta(tenant_id, contact_id, session_id);
    let identity = contact_identity_for(tenant_id, contact_id);
    let channel_ref = slack_channel_ref("1712668800.000100");
    let mismatched_session = SessionId::new();

    let result = initialize_contact_session_atomic(
        service.store.as_ref(),
        &service.pool,
        meta,
        identity,
        SessionChannelBindingId::new(),
        binding_update(tenant_id, contact_id, mismatched_session, &channel_ref),
        session_created_event(tenant_id, contact_id, &channel_ref),
    )
    .await;
    assert!(
        result.is_err(),
        "a late binding failure must abort session creation"
    );

    assert_eq!(
        count_sessions(&service, session_id).await?,
        0,
        "the session insert must not survive a later failure in the same transaction"
    );
    assert_eq!(count_events(&service, session_id).await?, 0);
    assert_eq!(count_session_tuples(&service, session_id).await?, 0);

    cleanup(&database_url, &schema_name).await
}

#[tokio::test]
async fn change_contact_session_channel_atomic_is_idempotent_on_replay_db() -> Result<()> {
    // Pins: replaying a channel change with the same stable binding id produces
    // exactly one new binding and one SessionChannelChanged event.
    let (service, database_url, schema_name) = test_service().await?;
    install_authz_outbox(&service).await?;
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session_id = SessionId::new();
    insert_verified_contact(&service, tenant_id, contact_id).await?;
    let initial_channel = slack_channel_ref("1712668800.000100");
    initialize_contact_session_atomic(
        service.store.as_ref(),
        &service.pool,
        contact_session_meta(tenant_id, contact_id, session_id),
        contact_identity_for(tenant_id, contact_id),
        SessionChannelBindingId::new(),
        binding_update(tenant_id, contact_id, session_id, &initial_channel),
        session_created_event(tenant_id, contact_id, &initial_channel),
    )
    .await
    .map_err(into_anyhow)?;

    let new_channel = slack_channel_ref("1712668900.000200");
    let binding_id = SessionChannelBindingId::new();
    for _ in 0..2 {
        let changed_event = Event::SessionChannelChanged {
            from: initial_channel.channel(),
            to: new_channel.channel(),
            contact_id: Some(contact_id),
            from_binding_id: None,
            to_binding_id: Some(binding_id),
            changed_by: Some(SessionActorRef::Contact { id: contact_id }),
            reason: Some("switch".to_string()),
        };
        change_contact_session_channel_atomic(
            service.store.as_ref(),
            &service.pool,
            binding_id,
            binding_update(tenant_id, contact_id, session_id, &new_channel),
            changed_event,
        )
        .await
        .map_err(into_anyhow)?;
    }

    // SessionCreated (1) + exactly one SessionChannelChanged despite the replay.
    assert_eq!(count_events(&service, session_id).await?, 2);
    let binding_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_channel_bindings WHERE id = $1")
            .bind(binding_id.0)
            .fetch_one(service.store.pool())
            .await?;
    assert_eq!(binding_rows, 1, "the replayed binding id is inserted once");
    let active = service
        .store
        .get_active_session_channel_binding(session_id)
        .await
        .map_err(into_anyhow)?
        .expect("session should have an active binding");
    assert_eq!(active.binding_id, binding_id);

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
        .store
        .emit_event_record(
            session_id,
            Event::UserMessage {
                text: "first".to_string(),
                attachments: vec![],
            },
            None,
        )
        .await
        .map(|record| record.sequence_num)
        .map_err(into_anyhow)?;
    let seq1 = service
        .store
        .emit_event_record(
            session_id,
            Event::UserMessage {
                text: "second".to_string(),
                attachments: vec![],
            },
            None,
        )
        .await
        .map(|record| record.sequence_num)
        .map_err(into_anyhow)?;
    let seq2 = service
        .store
        .emit_event_record(
            session_id,
            Event::UserMessage {
                text: "third".to_string(),
                attachments: vec![],
            },
            None,
        )
        .await
        .map(|record| record.sequence_num)
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
            .store
            .emit_event_record(
                session_id,
                Event::UserMessage {
                    text: format!("message {index}"),
                    attachments: vec![],
                },
                None,
            )
            .await
            .map_err(into_anyhow)?;
    }

    let events = service
        .store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(3),
                to_seq: Some(7),
                event_types: None,
                limit: None,
            },
        )
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
        .store
        .update_status(session_id, SessionStatus::Completed)
        .await
        .map_err(into_anyhow)?;
    let session = service
        .store
        .get_session(session_id)
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
        .store
        .emit_event_record(
            session_id,
            Event::UserMessage {
                text: "Fix the OAuth refresh token bug".to_string(),
                attachments: vec![],
            },
            None,
        )
        .await
        .map_err(into_anyhow)?;
    service
        .store
        .emit_event_record(
            session_id,
            Event::UserMessage {
                text: "Debug the refresh-token rotation failure".to_string(),
                attachments: vec![],
            },
            None,
        )
        .await
        .map_err(into_anyhow)?;

    let events = service
        .store
        .search_events("refresh-token", EventFilter::default())
        .await
        .map_err(into_anyhow)?;

    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::UserMessage { text, .. } if text.contains("refresh-token")
    )));

    cleanup(&database_url, &schema_name).await
}
